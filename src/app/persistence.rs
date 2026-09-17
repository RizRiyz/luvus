//! Session save admission and completion, owned by the app event loop.
use super::*;

impl App {
    /// At most one captured session waits for I/O. Clearing dirty at admission
    /// separates later mutations from this snapshot; acknowledgement never clears
    /// a newer dirty flag. Failure restores the retry intent.
    pub(crate) fn schedule_session_save(&mut self) {
        if self.session_save_inflight {
            return;
        }
        let capture = persist::capture_session(self);
        let accepted = self.io_jobs.submit(self.app_tx.clone(), move || {
            let saved = capture.write();
            Box::new(move |app| {
                app.session_save_inflight = false;
                if !saved {
                    app.session_dirty = true;
                    app.show_toast("session save failed; changes will be retried");
                }
                !saved
            })
        });
        if accepted.is_ok() {
            self.session_save_inflight = true;
            self.session_dirty = false;
            self.persist_session_now = false;
        }
    }

    /// The final capture follows every earlier accepted write on the same worker.
    /// No synchronous fallback can race an old delayed write after a timeout.
    pub(crate) fn finish_session_persistence(&mut self) {
        // Start the lazy worker if needed. A full queue still has a live worker;
        // its reserved shutdown slot remains available.
        let _ = self
            .io_jobs
            .submit(self.app_tx.clone(), || Box::new(|_| false));
        let capture = persist::capture_session(self);
        let config = self.final_config_save();
        let automation = self
            .automation_save_pending()
            .then(|| self.automation.clone());
        if !self.io_jobs.finish(Duration::from_secs(2), move || {
            let config_saved = config();
            let automation_saved = automation.is_none_or(|snapshot| snapshot.save().is_ok());
            capture.write() && config_saved && automation_saved
        }) {
            eprintln!("Luvus: final session save failed or exceeded the shutdown deadline");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn completion(rx: &mpsc::Receiver<AppEvent>, app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let ev = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("save completion");
            if let AppEvent::IoCompleted(done) = ev {
                done.apply(app);
                return;
            }
        }
    }

    #[test]
    fn session_ack_preserves_newer_dirty_state_and_final_write_is_last() {
        let _env = persist::test_env("session-save-order");
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.workspaces[0].name = "first".into();
        app.schedule_session_save();
        assert!(app.session_save_inflight);
        assert!(!app.session_dirty);
        app.workspaces[0].name = "latest".into();
        app.session_dirty = true;
        completion(&rx, &mut app);
        assert!(app.session_dirty, "old ack cannot clear new changes");
        assert!(!app.session_save_inflight);
        app.schedule_session_save();
        app.workspaces[0].name = "final".into();
        app.finish_session_persistence();
        assert_eq!(persist::load().unwrap().workspaces[0].name, "final");
    }

    #[test]
    fn failed_session_write_keeps_retry_intent_without_immediate_retry_loop() {
        let _env = persist::test_env("session-save-failure");
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        std::fs::create_dir_all(persist::session_dir().join("session.json")).unwrap();
        app.persist_session_now = true;
        app.schedule_session_save();
        completion(&rx, &mut app);
        assert!(app.session_dirty);
        assert!(
            !app.persist_session_now,
            "failed write retries at debounce cadence"
        );
        assert!(!app.session_save_inflight);
        app.drain_io_jobs();
    }

    #[test]
    fn empty_session_capture_preserves_explicit_closed_workspaces() {
        let _env = persist::test_env("session-save-empty");
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.workspaces.clear();
        app.closed_workspace_paths
            .push(PathBuf::from("/closed/project"));
        app.schedule_session_save();
        completion(&rx, &mut app);
        let snapshot = persist::load().unwrap();
        assert_eq!(snapshot.closed_workspace_paths, app.closed_workspace_paths);
        app.drain_io_jobs();
    }
}
