//! Process-wide PTY child waiter.
//!
//! One `luvus-pty-reaper` thread owns every pane child. An empty list blocks on
//! the registration channel. A live child blocks on SIGCHLD (Unix) or process
//! handles (Windows), then `try_wait`s. There is no idle timer.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::thread;

use crate::event::AppEvent;
use crate::ids::PaneId;

struct ReaperEntry {
    id: PaneId,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    child_exited: Arc<AtomicBool>,
    app_tx: Sender<AppEvent>,
}

struct Reaper {
    tx: Sender<ReaperEntry>,
    wake: Arc<ReaperWake>,
}

static CHILD_REAPER: OnceLock<Reaper> = OnceLock::new();

#[cfg(test)]
pub(super) static CHILD_REAPER_STARTS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Hand a child to the process-wide reaper. Pane I/O remains isolated behind
/// its platform backend; exit is observed from one waiter thread.
pub(super) fn register_child_reaper(
    id: PaneId,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    child_exited: Arc<AtomicBool>,
    app_tx: Sender<AppEvent>,
) {
    let reaper = CHILD_REAPER.get_or_init(start_reaper);
    let entry = ReaperEntry {
        id,
        child,
        child_exited,
        app_tx,
    };
    if let Err(error) = reaper.tx.send(entry) {
        // A panic in the shared reaper must not leave an unreaped child. This
        // fallback is deliberately exceptional; the normal path stays at one
        // waiter thread for the whole server.
        let mut entry = error.0;
        let _ = thread::Builder::new()
            .name("luvus-pty-reaper-fallback".to_string())
            .spawn(move || {
                let _ = entry.child.wait();
                finish_child(entry);
            });
        return;
    }
    reaper.wake.signal();
}

fn start_reaper() -> Reaper {
    let (tx, rx) = mpsc::channel();
    let wake = Arc::new(ReaperWake::new().expect("failed to create the PTY child reaper wake"));
    #[cfg(unix)]
    wake.install_sigchld();
    let thread_wake = Arc::clone(&wake);
    thread::Builder::new()
        .name("luvus-pty-reaper".to_string())
        .spawn(move || child_reaper_loop(rx, thread_wake))
        .expect("failed to start the PTY child reaper");
    #[cfg(test)]
    CHILD_REAPER_STARTS.fetch_add(1, Ordering::SeqCst);
    Reaper { tx, wake }
}

fn child_reaper_loop(rx: Receiver<ReaperEntry>, wake: Arc<ReaperWake>) {
    let mut children = Vec::<ReaperEntry>::new();
    loop {
        if children.is_empty() {
            match rx.recv() {
                Ok(entry) => children.push(entry),
                Err(_) => break,
            }
        } else {
            match wake.wait_for_exit_or_registration(&children) {
                Ok(()) => {}
                Err(_) => {
                    // The wake primitive failed. Block on the next registration
                    // so this thread cannot spin, then try_wait listed children.
                    match rx.recv() {
                        Ok(entry) => children.push(entry),
                        Err(_) => break,
                    }
                }
            }
        }
        children.extend(rx.try_iter());
        reap_finished(&mut children);
    }
}

fn reap_finished(children: &mut Vec<ReaperEntry>) {
    let mut index = 0;
    while index < children.len() {
        if child_poll_finished(children[index].child.try_wait()) {
            let entry = children.swap_remove(index);
            finish_child(entry);
        } else {
            index += 1;
        }
    }
}

#[inline]
pub(super) fn child_poll_finished(
    result: std::io::Result<Option<portable_pty::ExitStatus>>,
) -> bool {
    matches!(result, Ok(Some(_)))
}

fn finish_child(entry: ReaperEntry) {
    // Publish exit before notifying the app. If the app immediately drops the
    // pane, `Drop` must not signal a PID that the operating system may reuse.
    entry.child_exited.store(true, Ordering::SeqCst);
    let _ = entry.app_tx.send(AppEvent::PtyExit(entry.id));
}

struct ReaperWake {
    #[cfg(unix)]
    inner: UnixWake,
    #[cfg(windows)]
    inner: WindowsWake,
}

impl ReaperWake {
    fn new() -> io::Result<Self> {
        Ok(Self {
            #[cfg(unix)]
            inner: UnixWake::new()?,
            #[cfg(windows)]
            inner: WindowsWake::new()?,
        })
    }

    fn signal(&self) {
        self.inner.signal();
    }

    #[cfg(unix)]
    fn install_sigchld(&self) {
        self.inner.install_sigchld();
    }

    fn wait_for_exit_or_registration(&self, children: &[ReaperEntry]) -> io::Result<()> {
        self.inner.wait_for_exit_or_registration(children)
    }
}

#[cfg(unix)]
struct UnixWake {
    read: std::os::fd::OwnedFd,
    write: std::os::fd::OwnedFd,
}

#[cfg(unix)]
static SIGCHLD_WRITE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

#[cfg(unix)]
impl UnixWake {
    fn new() -> io::Result<Self> {
        use std::os::fd::{FromRawFd, OwnedFd};

        let mut fds = [-1; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a successful pipe call initializes two independently owned
        // descriptors. They are transferred into OwnedFd exactly once.
        let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        set_nonblocking_cloexec(fds[0])?;
        set_nonblocking_cloexec(fds[1])?;
        Ok(Self { read, write })
    }

    fn signal(&self) {
        use std::os::fd::AsRawFd;

        write_wake(self.write.as_raw_fd());
    }

    fn install_sigchld(&self) {
        use std::os::fd::AsRawFd;

        SIGCHLD_WRITE.store(self.write.as_raw_fd(), Ordering::Release);
        extern "C" fn on_sigchld(_sig: libc::c_int) {
            write_wake(SIGCHLD_WRITE.load(Ordering::Relaxed));
        }
        unsafe {
            // SAFETY: `action` is a fully initialized sigaction. The handler
            // only writes one byte to a pipe fd published before this call.
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_sigchld as extern "C" fn(libc::c_int) as libc::sighandler_t;
            action.sa_flags = libc::SA_NOCLDSTOP;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) != 0 {
                panic!(
                    "failed to install SIGCHLD handler for the PTY reaper: {}",
                    io::Error::last_os_error()
                );
            }
        }
    }

    fn wait_for_exit_or_registration(&self, _children: &[ReaperEntry]) -> io::Result<()> {
        use std::os::fd::AsRawFd;

        let mut fd = libc::pollfd {
            fd: self.read.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            // SAFETY: `fd` is this pipe's read end, valid for the lifetime of
            // `self`. Timeout -1 blocks until the pipe is readable.
            let result = unsafe { libc::poll(&mut fd, 1, -1) };
            if result < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            drain_wake(self.read.as_raw_fd());
            return Ok(());
        }
    }
}

#[cfg(unix)]
fn write_wake(fd: std::os::fd::RawFd) {
    if fd < 0 {
        return;
    }
    let byte = [1u8];
    // The pipe is nonblocking and acts as an edge coalescer. EAGAIN means a
    // prior byte already guarantees that poll will wake.
    let _ = unsafe { libc::write(fd, byte.as_ptr().cast(), byte.len()) };
}

#[cfg(unix)]
fn drain_wake(fd: std::os::fd::RawFd) {
    let mut bytes = [0u8; 64];
    loop {
        let count = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
        if count <= 0 {
            break;
        }
    }
}

#[cfg(unix)]
fn set_nonblocking_cloexec(fd: std::os::fd::RawFd) -> io::Result<()> {
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

#[cfg(windows)]
struct WindowsWake {
    event: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
// SAFETY: `event` is a kernel object. `SetEvent` / `WaitForMultipleObjects` are
// thread-safe. `CloseHandle` runs once when the last `Arc` drops (the
// process-lifetime `OnceLock`).
unsafe impl Send for WindowsWake {}
#[cfg(windows)]
unsafe impl Sync for WindowsWake {}

#[cfg(windows)]
impl WindowsWake {
    fn new() -> io::Result<Self> {
        use windows_sys::Win32::System::Threading::CreateEventW;

        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        if event.is_null() || event == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { event })
    }

    fn signal(&self) {
        use windows_sys::Win32::System::Threading::SetEvent;

        unsafe {
            let _ = SetEvent(self.event);
        }
    }

    fn wait_for_exit_or_registration(&self, children: &[ReaperEntry]) -> io::Result<()> {
        use windows_sys::Win32::Foundation::{HANDLE, WAIT_FAILED};
        use windows_sys::Win32::System::Threading::{WaitForMultipleObjects, INFINITE};

        // WaitForMultipleObjects accepts at most 64 handles. Slot 0 is the
        // registration event; remaining slots are live children. Extra children
        // are still `try_wait`ed after any wake; they are not wait sources.
        const MAX_WAIT: usize = 64;
        let mut handles = [std::ptr::null_mut::<std::ffi::c_void>(); MAX_WAIT];
        handles[0] = self.event;
        let mut count = 1usize;
        for entry in children {
            if count == MAX_WAIT {
                break;
            }
            if let Some(handle) = entry.child.as_raw_handle() {
                handles[count] = handle;
                count += 1;
            }
        }
        let status = unsafe {
            WaitForMultipleObjects(count as u32, handles.as_ptr() as *const HANDLE, 0, INFINITE)
        };
        if status == WAIT_FAILED {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(windows)]
impl Drop for WindowsWake {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ReaperWake;
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn reaper_wake_blocks_until_signaled() {
        let wake = Arc::new(ReaperWake::new().expect("wake pipe"));
        let waiter = Arc::clone(&wake);
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            ready_tx.send(()).unwrap();
            waiter
                .wait_for_exit_or_registration(&[])
                .expect("wait for wake");
        });
        ready_rx.recv().unwrap();
        // Longer than the old 50ms try_wait poll. A timer would return here.
        thread::sleep(Duration::from_millis(200));
        assert!(
            !thread.is_finished(),
            "the reaper wake returned without a child exit or registration"
        );
        wake.signal();
        thread.join().expect("waiter thread");
    }
}
