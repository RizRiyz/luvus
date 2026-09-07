//! Nonblocking admission shared by every producer and every writer stage.
use std::sync::{mpsc, Arc, Mutex, OnceLock};

use crate::{event::AppEvent, ids::PaneId};

use super::InputAction;

pub(crate) const MAX_QUEUED_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_QUEUED_ACTIONS: usize = 4096;

#[derive(Default)]
struct Usage {
    bytes: usize,
    actions: usize,
    notified: bool,
}

#[derive(Default)]
struct Budget {
    usage: Mutex<Usage>,
    notice: OnceLock<(PaneId, mpsc::Sender<AppEvent>)>,
}

/// Travels with the action until its final byte is written or cancelled.
pub(super) struct Reservation {
    budget: Arc<Budget>,
    bytes: usize,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut usage = self.budget.usage.lock().unwrap_or_else(|e| e.into_inner());
        usage.bytes -= self.bytes;
        usage.actions -= 1;
    }
}

pub(super) struct QueuedInput {
    pub action: InputAction,
    pub reservation: Reservation,
}

enum Sink {
    Writer(mpsc::Sender<QueuedInput>),
    #[cfg(test)]
    Fixture(mpsc::Sender<InputAction>),
}

#[derive(Clone)]
pub(crate) struct InputSender {
    sink: Arc<Sink>,
    budget: Arc<Budget>,
    #[cfg(unix)]
    wake: Arc<OnceLock<Arc<super::io::unix_actor::WakePipe>>>,
}

impl InputSender {
    pub(super) fn channel() -> (Self, mpsc::Receiver<QueuedInput>) {
        let (tx, rx) = mpsc::channel();
        (
            Self {
                sink: Arc::new(Sink::Writer(tx)),
                budget: Arc::new(Budget::default()),
                #[cfg(unix)]
                wake: Arc::new(OnceLock::new()),
            },
            rx,
        )
    }

    #[cfg(unix)]
    pub(super) fn wake_slot(&self) -> Arc<OnceLock<Arc<super::io::unix_actor::WakePipe>>> {
        self.wake.clone()
    }

    pub(super) fn set_notice(&self, id: PaneId, tx: mpsc::Sender<AppEvent>) {
        let _ = self.budget.notice.set((id, tx));
    }

    pub(super) fn acknowledge_rejection(&self) {
        self.budget
            .usage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .notified = false;
    }

    pub(super) fn wake(&self) {
        #[cfg(unix)]
        if let Some(wake) = self.wake.get() {
            wake.wake();
        }
    }

    pub(crate) fn send(&self, action: InputAction) -> Result<(), &'static str> {
        // Charge capacity, not length: retained Vec spare capacity is memory too.
        // Submit also reserves its future Enter before anything is accepted.
        let bytes = match &action {
            InputAction::Bytes(bytes) => bytes.capacity(),
            InputAction::Submit { paste, .. } => paste.capacity().saturating_add(1),
        };
        let mut usage = self.budget.usage.lock().unwrap_or_else(|e| e.into_inner());
        if bytes > MAX_QUEUED_BYTES.saturating_sub(usage.bytes)
            || usage.actions >= MAX_QUEUED_ACTIONS
        {
            if !usage.notified {
                if let Some((id, tx)) = self.budget.notice.get() {
                    usage.notified = tx.send(AppEvent::PtyInputRejected(*id)).is_ok();
                }
            }
            return Err("target pane input queue is full; input was not queued");
        }
        usage.bytes += bytes;
        usage.actions += 1;
        let reservation = Reservation {
            budget: self.budget.clone(),
            bytes,
        };
        // Admission and channel ordering share the same short lock. No I/O or
        // waiting for capacity occurs here. Drop reservations outside the lock.
        let result = match self.sink.as_ref() {
            Sink::Writer(tx) => tx
                .send(QueuedInput {
                    action,
                    reservation,
                })
                .map_err(|e| e.0),
            #[cfg(test)]
            Sink::Fixture(tx) => {
                let result = tx.send(action);
                drop(usage);
                drop(reservation);
                return result.map_err(|_| "target pane closed before input was queued");
            }
        };
        drop(usage);
        if result.is_err() {
            return Err("target pane closed before input was queued");
        }
        self.wake();
        Ok(())
    }
}

#[cfg(test)]
impl From<mpsc::Sender<InputAction>> for InputSender {
    fn from(tx: mpsc::Sender<InputAction>) -> Self {
        Self {
            sink: Arc::new(Sink::Fixture(tx)),
            budget: Arc::new(Budget::default()),
            #[cfg(unix)]
            wake: Arc::new(OnceLock::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_counts_capacity_and_reservations_survive_channel_drain() {
        let (tx, rx) = InputSender::channel();
        tx.send(InputAction::Bytes(Vec::with_capacity(MAX_QUEUED_BYTES)))
            .unwrap();
        let queued = rx.recv().unwrap();
        assert!(tx.send(InputAction::Bytes(vec![1])).is_err());
        drop(queued);
        tx.send(InputAction::Bytes(vec![1])).unwrap();
        drop(rx);
        assert_eq!(tx.budget.usage.lock().unwrap().bytes, 0);
        assert!(tx.send(InputAction::Bytes(vec![1])).is_err());
        assert_eq!(tx.budget.usage.lock().unwrap().actions, 0);
    }

    #[test]
    fn concurrent_empty_actions_are_bounded_and_overflow_is_coalesced() {
        let (tx, rx) = InputSender::channel();
        let (events, notices) = mpsc::channel();
        tx.set_notice(PaneId(1), events);
        let accepted: usize = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    let tx = &tx;
                    scope.spawn(move || {
                        (0..2000)
                            .filter(|_| tx.send(InputAction::Bytes(Vec::new())).is_ok())
                            .count()
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .sum()
        });
        assert_eq!(accepted, MAX_QUEUED_ACTIONS);
        assert!(matches!(
            notices.try_recv().unwrap(),
            AppEvent::PtyInputRejected(PaneId(1))
        ));
        assert!(notices.try_recv().is_err());
        drop(rx);
        assert_eq!(tx.budget.usage.lock().unwrap().actions, 0);
    }

    #[test]
    fn submit_reserves_enter_before_accepting_any_text() {
        let (tx, rx) = InputSender::channel();
        assert!(tx
            .send(InputAction::Submit {
                paste: vec![0; MAX_QUEUED_BYTES],
                settle: std::time::Duration::ZERO
            })
            .is_err());
        assert!(rx.try_recv().is_err());
        assert_eq!(tx.budget.usage.lock().unwrap().actions, 0);
        tx.send(InputAction::Submit {
            paste: vec![0; MAX_QUEUED_BYTES - 1],
            settle: std::time::Duration::ZERO,
        })
        .unwrap();
        assert!(tx.send(InputAction::Bytes(vec![1])).is_err());
    }
}
