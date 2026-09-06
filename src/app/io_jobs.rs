//! One lazy, bounded executor for app-owned filesystem work.
//!
//! Jobs own immutable inputs. Only their completions may mutate `App`, on its
//! existing event loop. Capacity includes completed but unapplied results.

use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use super::App;
use crate::event::AppEvent;

const MAX_JOBS: usize = 8;
type Apply = Box<dyn FnOnce(&mut App) -> bool + Send>;
type Work = Box<dyn FnOnce() -> Apply + Send>;

#[derive(Default)]
struct Budget {
    used: Mutex<usize>,
}

struct Permit(Arc<Budget>);

impl Drop for Permit {
    fn drop(&mut self) {
        *self.0.used.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
    }
}

pub(crate) struct Completion {
    apply: Apply,
    _permit: Permit,
}

impl Completion {
    pub(super) fn apply(self, app: &mut App) -> bool {
        let Self { apply, _permit } = self;
        drop(_permit);
        apply(app)
    }
}

enum Message {
    Job(Work, Permit),
    Drain(mpsc::Sender<()>),
}

#[derive(Default)]
pub(super) struct IoJobs {
    sender: Option<mpsc::Sender<Message>>,
    budget: Arc<Budget>,
    closing: bool,
}

impl IoJobs {
    /// Nonblocking admission. Callers retain dirty/pending intent on rejection.
    /// Each caller must bound its input and admit at most one large snapshot.
    pub(super) fn submit(
        &mut self,
        events: mpsc::Sender<AppEvent>,
        work: impl FnOnce() -> Apply + Send + 'static,
    ) -> Result<(), &'static str> {
        if self.closing {
            return Err("filesystem worker is shutting down");
        }
        let mut used = self.budget.used.lock().unwrap_or_else(|e| e.into_inner());
        if *used >= MAX_JOBS {
            return Err("filesystem work queue is full");
        }
        if self.sender.is_none() {
            let (sender, receiver) = mpsc::channel();
            std::thread::Builder::new()
                .name("luvus-io".into())
                .spawn(move || {
                    while let Ok(message) = receiver.recv() {
                        match message {
                            Message::Job(work, permit) => {
                                // A failed job must not strand unrelated admitted work.
                                let apply =
                                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                                        .unwrap_or_else(|_| {
                                            Box::new(|app: &mut App| {
                                                app.show_toast(
                                                    "filesystem job failed unexpectedly",
                                                );
                                                true
                                            })
                                        });
                                let _ = events.send(AppEvent::IoCompleted(Completion {
                                    apply,
                                    _permit: permit,
                                }));
                            }
                            Message::Drain(reply) => {
                                let _ = reply.send(());
                                break;
                            }
                        }
                    }
                })
                .map_err(|_| "could not start filesystem worker")?;
            self.sender = Some(sender);
        }
        *used += 1;
        drop(used);
        self.sender
            .as_ref()
            .expect("worker installed")
            .send(Message::Job(Box::new(work), Permit(self.budget.clone())))
            .map_err(|_| "filesystem worker unavailable")
    }

    /// Shutdown-only FIFO barrier. Never wait on storage in the interactive loop.
    /// OS filesystem calls cannot safely be cancelled; timeout reports uncertainty.
    pub(super) fn drain(&mut self, timeout: Duration) -> bool {
        self.closing = true;
        let Some(sender) = self.sender.take() else {
            return true;
        };
        let (tx, rx) = mpsc::channel();
        sender.send(Message::Drain(tx)).is_ok() && rx.recv_timeout(timeout).is_ok()
    }
}

impl App {
    pub(crate) fn drain_io_jobs(&mut self) {
        if !self.io_jobs.drain(Duration::from_secs(2)) {
            eprintln!("Luvus: filesystem work did not finish within the shutdown deadline");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_includes_unapplied_completions_and_drain_is_fifo() {
        let (tx, rx) = mpsc::channel();
        let mut jobs = IoJobs::default();
        let order = Arc::new(Mutex::new(Vec::new()));
        for i in 0..MAX_JOBS {
            let order = order.clone();
            jobs.submit(tx.clone(), move || {
                order.lock().unwrap().push(i);
                Box::new(|_| false)
            })
            .unwrap();
        }
        assert!(jobs.submit(tx.clone(), || Box::new(|_| false)).is_err());
        let completion = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(jobs.submit(tx.clone(), || Box::new(|_| false)).is_err());
        drop(completion);
        jobs.submit(tx, || Box::new(|_| false)).unwrap();
        assert!(jobs.drain(Duration::from_secs(2)));
        assert_eq!(*order.lock().unwrap(), (0..MAX_JOBS).collect::<Vec<_>>());
        assert!(jobs.sender.is_none());
    }

    #[test]
    fn blocked_job_does_not_block_admission_or_bounded_shutdown() {
        let (tx, _rx) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let mut jobs = IoJobs::default();
        jobs.submit(tx.clone(), move || {
            let _ = wait.recv();
            Box::new(|_| false)
        })
        .unwrap();
        jobs.submit(tx.clone(), || Box::new(|_| false)).unwrap();
        assert!(!jobs.drain(Duration::from_millis(10)));
        assert!(jobs.submit(tx, || Box::new(|_| false)).is_err());
        release.send(()).unwrap();
    }
}
