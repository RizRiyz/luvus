//! Coalesced settings writes with acknowledged baselines and bounded retry.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

struct Receipt {
    desired: crate::config::Config,
    explicit: Option<Value>,
    saved: Arc<AtomicBool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn next_completion(events: &mpsc::Receiver<AppEvent>) -> super::super::io_jobs::Completion {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("I/O completion")
            {
                AppEvent::IoCompleted(done) => return done,
                _ => continue,
            }
        }
    }

    #[test]
    fn settings_coalesce_behind_slow_work_without_losing_newer_changes() {
        let _env = persist::test_env("config-coalesced");
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let (release, wait) = mpsc::channel();
        app.io_jobs
            .submit(app.app_tx.clone(), move || {
                let _ = wait.recv();
                Box::new(|_| false)
            })
            .unwrap();
        app.config.agents_active_only = true;
        app.persist_config();
        app.config.agents_this_workspace = true;
        app.persist_config();
        assert!(app.config_persistence.inflight && app.config_persistence.dirty);
        release.send(()).unwrap();
        app.flush_config_for_test(&rx);
        let saved = crate::config::load();
        assert!(saved.agents_active_only && saved.agents_this_workspace);
    }

    #[test]
    fn later_delta_preserves_other_sessions_changes_after_old_write() {
        let _env = persist::test_env("config-ack-merge");
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.config.agents_active_only = true;
        app.persist_config();
        let first = next_completion(&rx);
        let mut external = crate::config::load();
        external.theme = "gruvbox-dark".into();
        crate::config::save(&external);
        app.config.agents_this_workspace = true;
        app.persist_config();
        first.apply(&mut app);
        app.flush_config_for_test(&rx);
        let saved = crate::config::load();
        assert_eq!(saved.theme, "gruvbox-dark");
        assert!(saved.agents_active_only && saved.agents_this_workspace);
    }

    #[test]
    fn failed_explicit_patch_is_retained_and_newer_patch_wins() {
        let _env = persist::test_env("config-retry-patch");
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let lock = persist::config_dir().join("config.lock");
        std::fs::create_dir_all(&lock).unwrap();
        app.persist_config_patch(&json!({"theme":"quattro-rally"}));
        next_completion(&rx).apply(&mut app);
        assert!(app.config_persistence.dirty);
        assert!(app.config_persistence.retry_at.is_some());
        app.config.theme = "gruvbox-dark".into();
        app.persist_config_patch(&json!({"theme":"gruvbox-dark"}));
        std::fs::remove_dir(lock).unwrap();
        app.schedule_config_save(Instant::now() + Duration::from_secs(3));
        app.flush_config_for_test(&rx);
        assert_eq!(crate::config::load().theme, "gruvbox-dark");
    }

    #[test]
    fn reload_waits_for_accepted_settings_write() {
        let _env = persist::test_env("config-reload-order");
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let stale = Box::new(app.config.clone());
        app.config.agents_this_workspace = true;
        app.persist_config();
        let (reply, response) = mpsc::channel();
        app.handle_event(AppEvent::ConfigReloaded {
            id: "reload".into(),
            config: stale,
            reply,
        });
        assert!(response.try_recv().is_err());
        app.flush_config_for_test(&rx);
        next_completion(&rx).apply(&mut app);
        let result: Value =
            serde_json::from_str(&response.recv_timeout(Duration::from_secs(1)).unwrap()).unwrap();
        assert_eq!(result["result"]["config"]["agents_this_workspace"], true);
        assert!(app.agents_this_workspace);
    }
}

#[derive(Default)]
pub(super) struct ConfigPersistence {
    pub(super) inflight: bool,
    pub(super) dirty: bool,
    pub(super) retry_at: Option<Instant>,
    explicit: Option<Value>,
    epoch: u64,
    receipt: Option<Receipt>,
    revision: u64,
    reloads: Vec<(String, Sender<String>)>,
}

// Compose explicit patches without losing null deletions. Arrays/scalars are
// atomic values; newer leaves always win over earlier unacknowledged intent.
fn merge_explicit(target: &mut Value, patch: &Value) {
    match (target, patch) {
        (Value::Object(target), Value::Object(patch)) => {
            for (key, value) in patch {
                match target.get_mut(key) {
                    Some(current) => merge_explicit(current, value),
                    None => {
                        target.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (target, patch) => *target = patch.clone(),
    }
}

impl App {
    #[cfg(test)]
    pub(crate) fn flush_config_for_test(&mut self, events: &std::sync::mpsc::Receiver<AppEvent>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.config_save_pending() {
            self.schedule_config_save(Instant::now());
            let event = events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("config persistence completion");
            if let AppEvent::IoCompleted(done) = event {
                done.apply(self);
            }
        }
    }
    pub(crate) fn persist_config(&mut self) {
        self.queue_config_patch(None);
    }

    pub(crate) fn persist_config_patch(&mut self, patch: &Value) {
        self.queue_config_patch(Some(patch));
    }

    fn queue_config_patch(&mut self, patch: Option<&Value>) {
        let state = &mut self.config_persistence;
        state.revision = state.revision.wrapping_add(1);
        state.dirty = true;
        if let Some(patch) = patch {
            if let Some(pending) = &mut state.explicit {
                merge_explicit(pending, patch);
            } else {
                state.explicit = Some(patch.clone());
            }
        }
        self.schedule_config_save(Instant::now());
    }

    pub(super) fn schedule_config_save(&mut self, now: Instant) {
        let state = &self.config_persistence;
        if state.inflight || !state.dirty || state.retry_at.is_some_and(|at| at > now) {
            return;
        }
        let desired = self.config.clone();
        let explicit = state.explicit.clone();
        let epoch = state.epoch;
        let saved_flag = Arc::new(AtomicBool::new(false));
        let worker_saved = saved_flag.clone();
        let receipt = Receipt {
            desired: desired.clone(),
            explicit: explicit.clone(),
            saved: saved_flag,
        };
        let request = crate::config::SaveRequest::new(
            self.config_baseline.clone(),
            desired.clone(),
            explicit.clone(),
        );
        let accepted = self.io_jobs.submit(self.app_tx.clone(), move || {
            let saved = request.write();
            worker_saved.store(saved, Ordering::Release);
            Box::new(move |app| {
                let state = &mut app.config_persistence;
                state.inflight = false;
                state.receipt = None;
                if state.epoch == epoch {
                    if saved {
                        app.config_baseline = desired;
                        state.retry_at = None;
                    } else {
                        state.dirty = true;
                        state.retry_at = Some(Instant::now() + Duration::from_secs(2));
                        if let Some(mut old) = explicit {
                            if let Some(newer) = &state.explicit {
                                merge_explicit(&mut old, newer);
                            }
                            state.explicit = Some(old);
                        }
                    }
                }
                if !saved {
                    app.show_toast("settings save failed; changes will be retried");
                }
                app.schedule_config_save(Instant::now());
                app.flush_config_reloads(saved);
                !saved
            })
        });
        let state = &mut self.config_persistence;
        if accepted.is_ok() {
            state.inflight = true;
            state.receipt = Some(receipt);
            state.dirty = false;
            state.explicit = None;
            state.retry_at = None;
        } else {
            state.retry_at = Some(now + Duration::from_secs(2));
        }
    }

    pub(crate) fn reset_config_baseline(&mut self) {
        self.config_baseline = self.config.clone();
        let state = &mut self.config_persistence;
        state.epoch = state.epoch.wrapping_add(1);
        state.revision = state.revision.wrapping_add(1);
        state.dirty = false;
        state.explicit = None;
        state.retry_at = None;
    }

    pub(super) fn config_save_pending(&self) -> bool {
        self.config_persistence.inflight || self.config_persistence.dirty
    }

    pub(super) fn defer_config_reload(&mut self, id: String, reply: Sender<String>) {
        if self.config_persistence.reloads.len() >= 8 {
            let _ = reply.send(
                json!({"id":id,"error":{"code":"busy","message":"settings reload queue is full"}})
                    .to_string(),
            );
            return;
        }
        self.config_persistence.reloads.push((id, reply));
    }

    fn flush_config_reloads(&mut self, saved: bool) {
        if self.config_persistence.reloads.is_empty() {
            return;
        }
        if !saved {
            for (id, reply) in self.config_persistence.reloads.drain(..) {
                let _ = reply.send(json!({"id":id,"error":{"code":"persistence_failed","message":"settings save failed; reload was not applied"}}).to_string());
            }
            return;
        }
        if self.config_save_pending() {
            return;
        }
        let reloads = self.config_persistence.reloads.clone();
        let revision = self.config_persistence.revision;
        let path = persist::config_dir().join("config.json");
        if self.io_jobs.submit(self.app_tx.clone(), move || {
            let config = std::fs::read_to_string(path).ok()
                .and_then(|text| serde_json::from_str(&text).ok())
                .map(crate::config::normalize_config).unwrap_or_default();
            Box::new(move |app| {
                let result = if app.config_persistence.revision != revision {
                    Err(("state_changed".into(), "settings changed during reload; retry reload".into()))
                } else { app.apply_socket_config(config, None) };
                for (id, reply) in reloads {
                    let response = match &result {
                        Ok(()) => json!({"id":id,"result":{"type":"config_reloaded","config":app.config}}),
                        Err((code, message)) => json!({"id":id,"error":{"code":code,"message":message}}),
                    };
                    let _ = reply.send(response.to_string());
                }
                result.is_ok()
            })
        }).is_ok() {
            self.config_persistence.reloads.clear();
        }
    }

    /// Final write follows the ordinary in-flight save on the same worker. Use
    /// its captured desired config as baseline so already-accepted fields are
    /// not needlessly written over another session's more recent changes.
    pub(super) fn final_config_save(&self) -> impl FnOnce() -> bool + Send + 'static {
        let state = &self.config_persistence;
        let mut failed_explicit = state.receipt.as_ref().and_then(|r| r.explicit.clone());
        if let Some(newer) = &state.explicit {
            if let Some(old) = &mut failed_explicit {
                merge_explicit(old, newer);
            } else {
                failed_explicit = Some(newer.clone());
            }
        }
        let failure = crate::config::SaveRequest::new(
            self.config_baseline.clone(),
            self.config.clone(),
            failed_explicit,
        );
        let success = state.receipt.as_ref().map(|receipt| {
            (
                receipt.saved.clone(),
                crate::config::SaveRequest::new(
                    receipt.desired.clone(),
                    self.config.clone(),
                    state.explicit.clone(),
                ),
            )
        });
        move || match success {
            Some((saved, request)) if saved.load(Ordering::Acquire) => request.write(),
            _ => failure.write(),
        }
    }
}
