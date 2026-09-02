use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::mpsc;
#[cfg(unix)]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};

use portable_pty::MasterPty;

use crate::event::AppEvent;
use crate::ids::PaneId;
use crate::terminal::vt::VtEngine;

use super::{InputAction, InputSender};

#[cfg(windows)]
mod split;
#[cfg(unix)]
pub(super) mod unix_actor;

pub(super) struct InputReceiver {
    receiver: mpsc::Receiver<InputAction>,
    #[cfg(unix)]
    wake: Arc<OnceLock<Arc<unix_actor::WakePipe>>>,
}

pub(super) fn input_channel() -> (InputSender, InputReceiver) {
    let (sender, receiver) = mpsc::channel();
    #[cfg(unix)]
    {
        let wake = Arc::new(OnceLock::new());
        let sender = InputSender::with_wake_slot(sender, wake.clone());
        (sender, InputReceiver { receiver, wake })
    }
    #[cfg(windows)]
    {
        (InputSender::from(sender), InputReceiver { receiver })
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn start(
    id: PaneId,
    master: &(dyn MasterPty + Send),
    input: InputReceiver,
    engine: Arc<Mutex<dyn VtEngine>>,
    app_tx: mpsc::Sender<AppEvent>,
    data_pending: Arc<AtomicBool>,
    content_revision: Arc<AtomicU64>,
    _cancelled: Arc<AtomicBool>,
) -> io::Result<()> {
    #[cfg(unix)]
    {
        unix_actor::start(
            id,
            master,
            input.receiver,
            input.wake,
            engine,
            app_tx,
            data_pending,
            content_revision,
            _cancelled,
        )
    }
    #[cfg(windows)]
    {
        split::start(
            id,
            master,
            input.receiver,
            engine,
            app_tx,
            data_pending,
            content_revision,
        )
    }
}
