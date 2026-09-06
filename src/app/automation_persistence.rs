//! Ordered automation checkpoints on the shared filesystem worker.
//!
//! One immutable ledger may be in flight. Later mutations coalesce, but an
//! external effect waits for acknowledgement of its own revision. A failed
//! checkpoint never grants permission to launch a worker or submit a prompt.
use super::*;

type AfterSave = Box<dyn FnOnce(&mut App, Result<(), String>) + Send>;

#[derive(Default)]
pub(super) struct AutomationPersistence {
    revision: u64,
    saved: u64,
    inflight: bool,
    retry_at: Option<Instant>,
    waiters: Vec<(u64, AfterSave)>,
}

impl App {
    pub(super) fn automation_save_pending(&self) -> bool {
        self.automation_persistence.revision != self.automation_persistence.saved
    }

    pub(super) fn persist_automation(&mut self) {
        // In-memory ledgers have no durability boundary (unit fixtures).
        if self.automation.persist_path.is_none() {
            return;
        }
        self.automation_persistence.revision += 1;
        self.schedule_automation_save(Instant::now());
    }

    pub(super) fn after_automation_save(
        &mut self,
        after: impl FnOnce(&mut App, Result<(), String>) + Send + 'static,
    ) {
        if !self.automation_save_pending() {
            after(self, Ok(()));
        } else if self.automation_persistence.waiters.len() >= 64 {
            after(self, Err("automation checkpoint wait queue is full".into()));
        } else {
            self.automation_persistence
                .waiters
                .push((self.automation_persistence.revision, Box::new(after)));
        }
    }

    pub(super) fn schedule_automation_save(&mut self, now: Instant) {
        let state = &self.automation_persistence;
        if state.inflight
            || !self.automation_save_pending()
            || state.retry_at.is_some_and(|deadline| now < deadline)
        {
            return;
        }
        let revision = state.revision;
        let snapshot = self.automation.clone();
        if self
            .io_jobs
            .submit(self.app_tx.clone(), move || {
                let result = snapshot.save().map_err(|error| error.to_string());
                Box::new(move |app| {
                    app.automation_persistence.inflight = false;
                    let (ready, pending): (Vec<_>, Vec<_>) =
                        std::mem::take(&mut app.automation_persistence.waiters)
                            .into_iter()
                            .partition(|(required, _)| result.is_err() || *required <= revision);
                    app.automation_persistence.waiters = pending;
                    if result.is_ok() {
                        app.automation_persistence.saved = revision;
                        app.automation_persistence.retry_at = None;
                    } else {
                        app.automation_persistence.retry_at =
                            Some(Instant::now() + Duration::from_secs(2));
                        // An API error must not turn into a later automatic launch
                        // when storage recovers. Terminalize unlaunched occurrences;
                        // retry only their bookkeeping, never the failed effect.
                        let pending = app.automation.pending_runs();
                        let now = crate::automation::unix_now();
                        for id in &pending {
                            let _ = app.automation.set_run_status(
                                id,
                                crate::automation::RunStatus::Failed,
                                Some("automation checkpoint failed before launch".into()),
                                now,
                            );
                            if let Some(run) = app.automation.run(id) {
                                app.emit_event(
                                    "automation.run_failed",
                                    json!({
                                        "automation_id":run.automation_id,
                                        "run_id":run.id,
                                        "code":"persistence_failed",
                                    }),
                                );
                            }
                        }
                        if !pending.is_empty() {
                            app.automation_persistence.revision += 1;
                        }
                        app.show_toast(
                            "automation save failed; dispatch paused until persistence recovers",
                        );
                    }
                    for (_, after) in ready {
                        after(app, result.clone());
                    }
                    app.schedule_automation_save(Instant::now());
                    if !app.automation_save_pending() {
                        app.start_pending_automation_runs(crate::automation::unix_now());
                    }
                    true
                })
            })
            .is_ok()
        {
            self.automation_persistence.inflight = true;
            self.automation_persistence.retry_at = None;
        } else {
            self.automation_persistence.retry_at = Some(now + Duration::from_secs(2));
        }
    }

    pub(super) fn automation_save_deadline(&self) -> Option<Instant> {
        self.automation_persistence.retry_at
    }

    pub(super) fn reply_after_automation_save(
        &mut self,
        req: crate::ipc::api::ApiRequest,
        response: String,
    ) {
        if !Self::is_automation_mutation(&req.method) || !self.automation_save_pending() {
            let _ = req.reply.send(response);
            return;
        }
        // Even an idempotent retry must not acknowledge an uncommitted record.
        let failed = serde_json::from_str::<Value>(&response)
            .is_ok_and(|value| value.get("error").is_some());
        if !failed {
            self.after_automation_save(move |_, result| {
                let response = match result {
                    Ok(()) => response,
                    Err(message) => json!({"id":req.id,"error":{
                        "code":"persistence_failed", "message":message
                    }})
                    .to_string(),
                };
                let _ = req.reply.send(response);
            });
        } else {
            let _ = req.reply.send(response);
        }
    }

    pub(super) fn is_automation_mutation(method: &str) -> bool {
        matches!(
            method,
            "automation.create"
                | "automation.update"
                | "automation.enable"
                | "automation.disable"
                | "automation.delete"
                | "automation.rebind"
                | "automation.run"
        )
    }

    pub(super) fn automation_admission_full(&self) -> bool {
        self.automation_persistence.waiters.len() >= 63
    }

    #[cfg(test)]
    pub(super) fn flush_automation_for_test(&mut self, rx: &std::sync::mpsc::Receiver<AppEvent>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.automation_save_pending() {
            if let AppEvent::IoCompleted(done) = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("automation checkpoint completion")
            {
                done.apply(self);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    };

    #[test]
    fn checkpoint_is_nonblocking_coalesces_and_fences_acknowledgements() {
        let _env = persist::test_env("automation-async-checkpoint");
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let path = persist::session_dir().join("automations.json");
        app.automation.persist_path = Some(path.clone());
        let (release, wait) = mpsc::channel();
        app.io_jobs
            .submit(app.app_tx.clone(), move || {
                wait.recv_timeout(Duration::from_secs(5)).unwrap();
                Box::new(|_| false)
            })
            .unwrap();
        app.persist_automation();
        let first = Arc::new(AtomicBool::new(false));
        let flag = first.clone();
        app.after_automation_save(move |_, result| {
            result.unwrap();
            flag.store(true, Ordering::SeqCst);
        });
        app.persist_automation();
        let latest = Arc::new(AtomicBool::new(false));
        let flag = latest.clone();
        app.after_automation_save(move |app, result| {
            result.unwrap();
            assert_eq!(app.automation_persistence.saved, 2);
            flag.store(true, Ordering::SeqCst);
        });
        assert!(!first.load(Ordering::SeqCst));
        assert!(!latest.load(Ordering::SeqCst));
        assert!(!path.exists());
        assert_eq!(app.dispatch("ping", &json!({})).unwrap()["type"], "pong");
        release.send(()).unwrap();
        app.flush_automation_for_test(&rx);
        assert!(first.load(Ordering::SeqCst));
        assert!(latest.load(Ordering::SeqCst));
        let _: crate::automation::AutomationState =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    }

    #[test]
    fn failed_checkpoint_rejects_effect_and_api_success_then_retries() {
        let _env = persist::test_env("automation-async-failure");
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let path = persist::session_dir().join("automations.json");
        std::fs::create_dir_all(&path).unwrap();
        app.automation.persist_path = Some(path.clone());
        let input = crate::automation::CreateAutomation {
            name: "failed admission".into(),
            enabled: true,
            trigger: crate::automation::Trigger::Once {
                at_utc: 4_000_000_000,
            },
            target: crate::automation::AutomationTarget::NewWorker,
            task: crate::automation::TaskTemplate {
                title: "failed admission".into(),
                prompt: "must not launch".into(),
                agent_id: "codex".into(),
                workspace_id: app.workspaces[0].id.clone(),
                mode: crate::orch::TaskWorkerMode::Workspace,
                access: crate::automation::AutomationAccess::ReadOnly,
                paths: Vec::new(),
                gate: None,
            },
            policy: crate::automation::AutomationPolicy::default(),
        };
        let definition = app.automation.create(input, None, 10).unwrap();
        let run = app
            .automation
            .request_run(&definition.id, None, 20)
            .unwrap();
        app.persist_automation();
        let (reply, response) = mpsc::channel();
        app.reply_after_automation_save(
            crate::ipc::api::ApiRequest {
                id: "mutation".into(),
                method: "automation.create".into(),
                params: json!({}),
                reply,
            },
            json!({"id":"mutation","result":{}}).to_string(),
        );
        assert!(response.try_recv().is_err());
        loop {
            if let AppEvent::IoCompleted(done) = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                done.apply(&mut app);
                break;
            }
        }
        let result: Value = serde_json::from_str(&response.try_recv().unwrap()).unwrap();
        assert_eq!(result["error"]["code"], "persistence_failed");
        assert!(app.automation_save_pending());
        assert!(!app.start_pending_automation_runs(crate::automation::unix_now()));
        assert!(app.automation_save_deadline().is_some());
        std::fs::remove_dir(path).unwrap();
        app.schedule_automation_save(Instant::now() + Duration::from_secs(3));
        app.flush_automation_for_test(&rx);
        assert!(!app.automation_save_pending());
        assert_eq!(
            app.automation.run(&run.id).unwrap().status,
            crate::automation::RunStatus::Failed
        );
        assert!(
            app.orch.tasks.is_empty(),
            "storage recovery must not launch the failed occurrence"
        );
    }
}
