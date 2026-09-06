use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::MasterPty;

use crate::event::AppEvent;
use crate::ids::PaneId;
use crate::terminal::vt::VtEngine;

use super::super::InputAction;

const IO_BUDGET: usize = 64 * 1024;

pub(crate) struct WakePipe {
    read: OwnedFd,
    write: OwnedFd,
}

impl WakePipe {
    pub(super) fn new() -> io::Result<Self> {
        let mut fds = [-1; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a successful pipe call initializes two independently owned
        // descriptors. They are transferred into OwnedFd exactly once.
        let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        set_nonblocking_cloexec(read.as_raw_fd())?;
        set_nonblocking_cloexec(write.as_raw_fd())?;
        Ok(Self { read, write })
    }

    pub(crate) fn wake(&self) {
        let byte = [1u8];
        // The pipe is nonblocking and acts as an edge coalescer. EAGAIN means a
        // prior byte already guarantees that poll will wake.
        let _ = unsafe { libc::write(self.write.as_raw_fd(), byte.as_ptr().cast(), byte.len()) };
    }

    fn drain(&self) {
        let mut bytes = [0u8; 64];
        loop {
            let count = unsafe {
                libc::read(
                    self.read.as_raw_fd(),
                    bytes.as_mut_ptr().cast(),
                    bytes.len(),
                )
            };
            if count <= 0 {
                break;
            }
        }
    }
}

fn set_nonblocking_cloexec(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let descriptor_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if descriptor_flags < 0
        || unsafe { libc::fcntl(fd, libc::F_SETFD, descriptor_flags | libc::FD_CLOEXEC) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn duplicate_nonblocking(fd: RawFd) -> io::Result<OwnedFd> {
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: F_DUPFD_CLOEXEC returned a new owned descriptor.
    let duplicate = unsafe { OwnedFd::from_raw_fd(duplicate) };
    set_nonblocking_cloexec(duplicate.as_raw_fd())?;
    Ok(duplicate)
}

enum PendingWrite {
    Bytes {
        bytes: Vec<u8>,
        offset: usize,
    },
    SubmitDelay {
        settle: Duration,
        deadline: Option<Instant>,
    },
}

impl PendingWrite {
    fn bytes(bytes: Vec<u8>) -> Option<Self> {
        (!bytes.is_empty()).then_some(Self::Bytes { bytes, offset: 0 })
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn start(
    id: PaneId,
    master: &(dyn MasterPty + Send),
    input: mpsc::Receiver<InputAction>,
    wake_slot: Arc<OnceLock<Arc<WakePipe>>>,
    engine: Arc<Mutex<dyn VtEngine>>,
    app_tx: mpsc::Sender<AppEvent>,
    data_pending: Arc<AtomicBool>,
    content_revision: Arc<AtomicU64>,
    cancelled: Arc<AtomicBool>,
) -> io::Result<()> {
    let source = master
        .as_raw_fd()
        .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "PTY has no Unix descriptor"))?;
    let master = duplicate_nonblocking(source)?;
    let wake = Arc::new(WakePipe::new()?);
    wake_slot.set(wake.clone()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "PTY actor wake pipe was already installed",
        )
    })?;
    thread::Builder::new()
        .name(format!("luvus-pty-actor-{}", id.0))
        .spawn(move || {
            actor_loop(
                id,
                master,
                input,
                wake,
                engine,
                app_tx,
                data_pending,
                content_revision,
                cancelled,
            );
        })?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn actor_loop(
    id: PaneId,
    master: OwnedFd,
    input: mpsc::Receiver<InputAction>,
    wake: Arc<WakePipe>,
    engine: Arc<Mutex<dyn VtEngine>>,
    app_tx: mpsc::Sender<AppEvent>,
    data_pending: Arc<AtomicBool>,
    content_revision: Arc<AtomicU64>,
    cancelled: Arc<AtomicBool>,
) {
    let mut pending = VecDeque::new();
    let mut read_buffer = [0u8; 8192];
    loop {
        wake.drain();
        drain_input(&input, &mut pending);
        if cancelled.load(Ordering::Acquire) {
            break;
        }

        let now = Instant::now();
        arm_or_finish_submit_delay(&mut pending, now);
        let wants_write = matches!(pending.front(), Some(PendingWrite::Bytes { .. }));
        let timeout = poll_timeout(&pending, now);
        let mut fds = [
            libc::pollfd {
                fd: master.as_raw_fd(),
                events: libc::POLLIN | if wants_write { libc::POLLOUT } else { 0 },
                revents: 0,
            },
            libc::pollfd {
                fd: wake.read.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) };
        if result < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if fds[1].revents & libc::POLLIN != 0 {
            wake.drain();
            drain_input(&input, &mut pending);
        }
        if cancelled.load(Ordering::Acquire) {
            break;
        }

        arm_or_finish_submit_delay(&mut pending, Instant::now());
        if fds[0].revents & libc::POLLOUT != 0
            && write_pending(master.as_raw_fd(), &mut pending).is_err()
        {
            break;
        }

        let terminal_events = fds[0].revents;
        if terminal_events & (libc::POLLIN | libc::POLLHUP) != 0 {
            match read_available(
                master.as_raw_fd(),
                &mut read_buffer,
                &engine,
                &content_revision,
            ) {
                Ok(ReadState::Data) => {
                    if !data_pending.swap(true, Ordering::AcqRel)
                        && app_tx.send(AppEvent::PtyData(id)).is_err()
                    {
                        break;
                    }
                }
                Ok(ReadState::WouldBlock) if terminal_events & libc::POLLHUP == 0 => {}
                Ok(ReadState::WouldBlock | ReadState::Eof) | Err(_) => break,
            }
        }
        if terminal_events & (libc::POLLERR | libc::POLLNVAL) != 0 {
            break;
        }
    }
    let _ = app_tx.send(AppEvent::PtyExit(id));
}

fn drain_input(input: &mpsc::Receiver<InputAction>, pending: &mut VecDeque<PendingWrite>) {
    while let Ok(action) = input.try_recv() {
        match action {
            InputAction::Bytes(bytes) => {
                if let Some(bytes) = PendingWrite::bytes(bytes) {
                    pending.push_back(bytes);
                }
            }
            InputAction::Submit { paste, settle } => {
                if let Some(paste) = PendingWrite::bytes(paste) {
                    pending.push_back(paste);
                }
                pending.push_back(PendingWrite::SubmitDelay {
                    settle,
                    deadline: None,
                });
            }
        }
    }
}

fn arm_or_finish_submit_delay(pending: &mut VecDeque<PendingWrite>, now: Instant) {
    let Some(PendingWrite::SubmitDelay { settle, deadline }) = pending.front_mut() else {
        return;
    };
    let due = *deadline.get_or_insert_with(|| now + *settle);
    if now >= due {
        pending.pop_front();
        pending.push_front(PendingWrite::Bytes {
            bytes: vec![b'\r'],
            offset: 0,
        });
    }
}

fn poll_timeout(pending: &VecDeque<PendingWrite>, now: Instant) -> libc::c_int {
    match pending.front() {
        Some(PendingWrite::SubmitDelay {
            deadline: Some(deadline),
            ..
        }) => deadline
            .saturating_duration_since(now)
            .as_millis()
            .max(1)
            .min(libc::c_int::MAX as u128) as libc::c_int,
        Some(PendingWrite::SubmitDelay { deadline: None, .. }) => 0,
        Some(PendingWrite::Bytes { .. }) => -1,
        None => -1,
    }
}

fn write_pending(fd: RawFd, pending: &mut VecDeque<PendingWrite>) -> io::Result<()> {
    let mut budget = IO_BUDGET;
    while budget > 0 {
        let Some(PendingWrite::Bytes { bytes, offset }) = pending.front_mut() else {
            break;
        };
        let remaining = &bytes[*offset..];
        let count =
            unsafe { libc::write(fd, remaining.as_ptr().cast(), remaining.len().min(budget)) };
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(());
            }
            return Err(error);
        }
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "PTY write returned zero",
            ));
        }
        *offset += count as usize;
        budget -= count as usize;
        if *offset == bytes.len() {
            pending.pop_front();
            arm_or_finish_submit_delay(pending, Instant::now());
        }
    }
    Ok(())
}

enum ReadState {
    Data,
    WouldBlock,
    Eof,
}

fn read_available(
    fd: RawFd,
    buffer: &mut [u8],
    engine: &Arc<Mutex<dyn VtEngine>>,
    content_revision: &AtomicU64,
) -> io::Result<ReadState> {
    let mut budget = IO_BUDGET;
    let mut read_any = false;
    while budget > 0 {
        let count = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len().min(budget)) };
        if count == 0 {
            return Ok(if read_any {
                ReadState::Data
            } else {
                ReadState::Eof
            });
        }
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(if read_any {
                    ReadState::Data
                } else {
                    ReadState::WouldBlock
                });
            }
            return if read_any {
                Ok(ReadState::Data)
            } else {
                Err(error)
            };
        }
        let count = count as usize;
        if let Ok(mut engine) = engine.lock() {
            engine.advance(&buffer[..count]);
            content_revision.fetch_add(1, Ordering::Release);
        }
        read_any = true;
        budget -= count;
    }
    Ok(ReadState::Data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_delay_preserves_fifo_order() {
        let mut pending = VecDeque::new();
        pending.push_back(PendingWrite::Bytes {
            bytes: b"prompt".to_vec(),
            offset: 0,
        });
        pending.push_back(PendingWrite::SubmitDelay {
            settle: Duration::from_millis(10),
            deadline: None,
        });
        pending.push_back(PendingWrite::Bytes {
            bytes: b"later".to_vec(),
            offset: 0,
        });
        assert!(matches!(pending.front(), Some(PendingWrite::Bytes { .. })));
        pending.pop_front();
        let start = Instant::now();
        arm_or_finish_submit_delay(&mut pending, start);
        assert!(matches!(
            pending.front(),
            Some(PendingWrite::SubmitDelay { .. })
        ));
        arm_or_finish_submit_delay(&mut pending, start + Duration::from_millis(11));
        assert!(matches!(
            pending.front(),
            Some(PendingWrite::Bytes { bytes, .. }) if bytes == b"\r"
        ));
    }

    #[test]
    fn zero_delay_submit_writes_paste_enter_and_following_bytes_in_order() {
        let mut sockets = [-1; 2];
        assert_eq!(
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sockets.as_mut_ptr()) },
            0
        );
        // SAFETY: socketpair returned two distinct owned descriptors.
        let writer = unsafe { OwnedFd::from_raw_fd(sockets[0]) };
        let reader = unsafe { OwnedFd::from_raw_fd(sockets[1]) };
        set_nonblocking_cloexec(writer.as_raw_fd()).unwrap();

        let mut pending = VecDeque::from([
            PendingWrite::Bytes {
                bytes: b"prompt".to_vec(),
                offset: 0,
            },
            PendingWrite::SubmitDelay {
                settle: Duration::ZERO,
                deadline: None,
            },
            PendingWrite::Bytes {
                bytes: b"later".to_vec(),
                offset: 0,
            },
        ]);
        write_pending(writer.as_raw_fd(), &mut pending).unwrap();
        assert!(pending.is_empty());

        let mut bytes = [0u8; 32];
        let count =
            unsafe { libc::read(reader.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) };
        assert_eq!(&bytes[..count as usize], b"prompt\rlater");
    }
}
