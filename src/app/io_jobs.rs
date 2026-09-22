//! One lazy, bounded executor for app-owned filesystem work.
//!
//! Jobs own immutable inputs. Only their completions may mutate `App`, on its
//! existing event loop. Capacity includes completed but unapplied results.

use std::sync::atomic::{AtomicBool, Ordering};
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
    Drain(Box<dyn FnOnce() -> bool + Send>, mpsc::Sender<bool>),
}

pub(super) struct ParallelJob {
    pub cancelled: Arc<AtomicBool>,
    pub handle: std::thread::JoinHandle<()>,
}

#[derive(Default)]
pub(super) struct IoJobs {
    sender: Option<mpsc::Sender<Message>>,
    budget: Arc<Budget>,
    parallel: Vec<ParallelJob>,
    closing: bool,
}

impl IoJobs {
    fn reap_parallel(&mut self) {
        let mut active = Vec::with_capacity(self.parallel.len());
        for job in self.parallel.drain(..) {
            if job.handle.is_finished() {
                let _ = job.handle.join();
            } else {
                active.push(job);
            }
        }
        self.parallel = active;
    }

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
        self.reap_parallel();
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
                            Message::Drain(final_work, reply) => {
                                let _ = reply.send(final_work());
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

    /// Admit one bounded long-running job without occupying the serial filesystem
    /// worker. The shared permit still caps all app-owned background work.
    pub(super) fn submit_parallel(
        &mut self,
        events: mpsc::Sender<AppEvent>,
        work: impl FnOnce(Arc<AtomicBool>) -> Apply + Send + 'static,
    ) -> Result<(), &'static str> {
        if self.closing {
            return Err("filesystem worker is shutting down");
        }
        self.reap_parallel();
        let mut used = self.budget.used.lock().unwrap_or_else(|e| e.into_inner());
        if *used >= MAX_JOBS {
            return Err("filesystem work queue is full");
        }
        *used += 1;
        let permit = Permit(self.budget.clone());
        drop(used);
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let handle = std::thread::Builder::new()
            .name("luvus-long-io".into())
            .spawn(move || {
                let apply = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    work(worker_cancelled)
                }))
                .unwrap_or_else(|_| {
                    Box::new(|app: &mut App| {
                        app.show_toast("background job failed unexpectedly");
                        true
                    })
                });
                let _ = events.send(AppEvent::IoCompleted(Completion {
                    apply,
                    _permit: permit,
                }));
            })
            .map_err(|_| "could not start background worker")?;
        self.parallel.push(ParallelJob { cancelled, handle });
        Ok(())
    }

    /// Shutdown-only FIFO barrier. Never wait on storage in the interactive loop.
    /// OS filesystem calls cannot safely be cancelled; timeout reports uncertainty.
    #[cfg(test)]
    pub(super) fn drain(&mut self, timeout: Duration) -> bool {
        self.finish(timeout, || true)
    }

    pub(super) fn finish(
        &mut self,
        timeout: Duration,
        final_work: impl FnOnce() -> bool + Send + 'static,
    ) -> bool {
        self.closing = true;
        for job in &self.parallel {
            job.cancelled.store(true, Ordering::Release);
        }
        let deadline = std::time::Instant::now() + timeout;
        let serial = if let Some(sender) = self.sender.take() {
            let (tx, rx) = mpsc::channel();
            sender
                .send(Message::Drain(Box::new(final_work), tx))
                .is_ok()
                && rx
                    .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                    .unwrap_or(false)
        } else {
            final_work()
        };
        let mut parallel = true;
        for job in self.parallel.drain(..) {
            while !job.handle.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if job.handle.is_finished() {
                let _ = job.handle.join();
            } else {
                parallel = false;
            }
        }
        serial && parallel
    }
}

impl App {
    #[cfg(test)]
    pub(crate) fn drain_io_jobs(&mut self) {
        if !self.io_jobs.drain(Duration::from_secs(2)) {
            eprintln!("Luvus: filesystem work did not finish within the shutdown deadline");
        }
    }
}

impl Drop for IoJobs {
    fn drop(&mut self) {
        // Covers early startup/test exits as well as the explicit server drain.
        // A completed explicit drain has already taken the sender.
        if self.sender.is_some() || !self.parallel.is_empty() {
            let _ = self.finish(Duration::from_secs(2), || true);
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
    fn parallel_jobs_are_cancelled_and_joined_on_shutdown() {
        let (tx, _rx) = mpsc::channel();
        let mut jobs = IoJobs::default();
        let stopped = Arc::new(AtomicBool::new(false));
        let observed = stopped.clone();
        jobs.submit_parallel(tx, move |cancelled| {
            while !cancelled.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(5));
            }
            observed.store(true, Ordering::Release);
            Box::new(|_| false)
        })
        .unwrap();
        assert!(jobs.drain(Duration::from_secs(2)));
        assert!(stopped.load(Ordering::Acquire));
        assert!(jobs.parallel.is_empty());
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
