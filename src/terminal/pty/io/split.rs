use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use portable_pty::MasterPty;

use crate::event::AppEvent;
use crate::ids::PaneId;
use crate::terminal::vt::VtEngine;

use super::super::{input::QueuedInput, read_loop, write_input_action};

#[allow(clippy::too_many_arguments)]
pub(super) fn start(
    id: PaneId,
    master: &(dyn MasterPty + Send),
    input: mpsc::Receiver<QueuedInput>,
    engine: Arc<Mutex<dyn VtEngine>>,
    app_tx: mpsc::Sender<AppEvent>,
    data_pending: Arc<AtomicBool>,
    content_revision: Arc<AtomicU64>,
) -> io::Result<()> {
    let mut writer = master
        .take_writer()
        .map_err(|error| io::Error::other(format!("take PTY writer: {error:#}")))?;
    thread::Builder::new()
        .name(format!("luvus-pty-writer-{}", id.0))
        .spawn(move || {
            while let Ok(QueuedInput {
                action,
                reservation,
            }) = input.recv()
            {
                if write_input_action(writer.as_mut(), action).is_err() {
                    break;
                }
                drop(reservation);
            }
        })?;

    let reader = master
        .try_clone_reader()
        .map_err(|error| io::Error::other(format!("clone PTY reader: {error:#}")))?;
    thread::Builder::new()
        .name(format!("luvus-pty-reader-{}", id.0))
        .spawn(move || read_loop(id, reader, engine, app_tx, data_pending, content_revision))?;
    Ok(())
}
