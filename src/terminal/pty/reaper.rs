//! Process-wide child waiter for PTYs and fire-and-forget helpers.
//!
//! One `luvus-pty-reaper` thread owns every child. An empty list blocks on
//! the registration channel. A live child blocks on SIGCHLD (Unix) or process
//! handles (Windows), then `try_wait`s. There is no idle timer.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use crate::event::AppEvent;
use crate::ids::PaneId;

struct ReaperEntry {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    completion: Option<PaneCompletion>,
}

struct PaneCompletion {
    id: PaneId,
    child_exited: Arc<AtomicBool>,
    app_tx: Sender<AppEvent>,
}

type PendingHelper = Arc<Mutex<Option<std::process::Child>>>;

enum Registration {
    Pane(ReaperEntry),
    Helper(PendingHelper),
}

impl Registration {
    fn into_entry(self) -> Option<ReaperEntry> {
        match self {
            Self::Pane(entry) => Some(entry),
            Self::Helper(pending) => {
                let child = pending
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take()?;
                Some(ReaperEntry {
                    child: Box::new(child),
                    completion: None,
                })
            }
        }
    }
}

struct Reaper {
    tx: Sender<Registration>,
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
    let reaper = get_reaper().expect("failed to prepare the child reaper");
    let entry = ReaperEntry {
        child,
        completion: Some(PaneCompletion {
            id,
            child_exited,
            app_tx,
        }),
    };
    if let Err(error) = reaper.tx.send(Registration::Pane(entry)) {
        // A panic in the shared reaper must not leave an unreaped child. This
        // fallback is deliberately exceptional; the normal path stays at one
        // waiter thread for the whole server.
        let mut entry = error
            .0
            .into_entry()
            .expect("pane registration owns a child");
        let _ = thread::Builder::new()
            .name("luvus-pty-reaper-fallback".to_string())
            .spawn(move || {
                let status = entry.child.wait().ok();
                finish_child(entry, status);
            });
        return;
    }
    reaper.wake.signal();
}

/// Register ownership before spawning, so neither setup nor channel failure
/// can lose an already-started helper. Command startup still runs on the caller
/// as with `Command::spawn`; all exit waiting happens on the shared reaper.
pub(crate) fn spawn_helper_reaped(command: &mut std::process::Command) -> Option<u32> {
    let reaper = get_reaper().ok()?;
    spawn_helper_reaped_with(command, |pending| {
        reaper
            .tx
            .send(Registration::Helper(pending))
            .map_err(|_| io::Error::other("child reaper disconnected"))?;
        reaper.wake.signal();
        Ok(())
    })
}

fn spawn_helper_reaped_with(
    command: &mut std::process::Command,
    register: impl FnOnce(PendingHelper) -> io::Result<()>,
) -> Option<u32> {
    let pending = Arc::new(Mutex::new(None));
    // The reaper waits for this lock only until command startup finishes, never
    // until the helper exits. No fallible handoff follows successful startup.
    let mut pending_child = pending.lock().unwrap_or_else(|error| error.into_inner());
    register(Arc::clone(&pending)).ok()?;
    let child = command.spawn().ok()?;
    let pid = child.id();
    *pending_child = Some(child);
    Some(pid)
}

fn get_reaper() -> io::Result<&'static Reaper> {
    static START_LOCK: Mutex<()> = Mutex::new(());
    if let Some(reaper) = CHILD_REAPER.get() {
        return Ok(reaper);
    }
    let _initializing = START_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    if CHILD_REAPER.get().is_none() {
        let _ = CHILD_REAPER.set(start_reaper()?);
    }
    Ok(CHILD_REAPER
        .get()
        .expect("initialized while holding START_LOCK"))
}

fn start_reaper() -> io::Result<Reaper> {
    start_reaper_with(|waiter| {
        thread::Builder::new()
            .name("luvus-pty-reaper".to_string())
            .spawn(waiter)
            .map(|_| ())
    })
}

fn start_reaper_with(
    start_waiter: impl FnOnce(Box<dyn FnOnce() + Send>) -> io::Result<()>,
) -> io::Result<Reaper> {
    let (tx, rx) = mpsc::channel();
    let wake = Arc::new(ReaperWake::new()?);
    let thread_wake = Arc::clone(&wake);
    #[cfg(unix)]
    let (ready_tx, ready_rx) = mpsc::sync_channel(0);
    start_waiter(Box::new(move || {
        #[cfg(unix)]
        {
            // Signal masks survive exec and are inherited by new threads.
            // Unblock SIGCHLD only in the process-lifetime reaper before it
            // relies on the handler's self-pipe.
            let _sigchld_unblocked = match SigchldUnblockGuard::new() {
                Ok(guard) => {
                    if ready_tx.send(Ok(())).is_err() {
                        return;
                    }
                    guard
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                    return;
                }
            };
            child_reaper_loop(rx, thread_wake);
        }
        #[cfg(windows)]
        child_reaper_loop(rx, thread_wake);
    }))?;
    #[cfg(unix)]
    {
        ready_rx
            .recv()
            .map_err(|_| io::Error::other("child reaper exited during signal-mask setup"))??;
        // Install only after thread startup succeeds: failure must not leave a
        // signal handler pointing at the closed wake pipe.
        wake.install_sigchld()?;
    }
    #[cfg(test)]
    CHILD_REAPER_STARTS.fetch_add(1, Ordering::SeqCst);
    Ok(Reaper { tx, wake })
}

fn child_reaper_loop(rx: Receiver<Registration>, wake: Arc<ReaperWake>) {
    let mut children = Vec::<ReaperEntry>::new();
    loop {
        if children.is_empty() {
            match rx.recv() {
                Ok(registration) => children.extend(registration.into_entry()),
                Err(_) => break,
            }
        } else {
            match wake.wait_for_exit_or_registration(&children) {
                Ok(()) => {}
                Err(_) => {
                    // The wake primitive failed. Block on the next registration
                    // so this thread cannot spin, then try_wait listed children.
                    match rx.recv() {
                        Ok(registration) => children.extend(registration.into_entry()),
                        Err(_) => break,
                    }
                }
            }
        }
        children.extend(rx.try_iter().filter_map(Registration::into_entry));
        reap_finished(&mut children);
    }
}

fn reap_finished(children: &mut Vec<ReaperEntry>) {
    let mut index = 0;
    while index < children.len() {
        if let Some(status) = child_poll_status(children[index].child.try_wait()) {
            let entry = children.swap_remove(index);
            finish_child(entry, Some(status));
        } else {
            index += 1;
        }
    }
}

#[inline]
pub(super) fn child_poll_status(
    result: std::io::Result<Option<portable_pty::ExitStatus>>,
) -> Option<portable_pty::ExitStatus> {
    result.ok().flatten()
}

fn finish_child(entry: ReaperEntry, status: Option<portable_pty::ExitStatus>) {
    // Publish exit before notifying the app. If the app immediately drops the
    // pane, `Drop` must not signal a PID that the operating system may reuse.
    if let Some(completion) = entry.completion {
        completion.child_exited.store(true, Ordering::SeqCst);
        let _ = completion.app_tx.send(AppEvent::PtyReaped(
            completion.id,
            status.map(Into::into).unwrap_or_default(),
        ));
    }
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
    fn install_sigchld(&self) -> io::Result<()> {
        self.inner.install_sigchld()
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
struct SigchldUnblockGuard {
    inherited: libc::sigset_t,
}

#[cfg(unix)]
impl SigchldUnblockGuard {
    fn new() -> io::Result<Self> {
        unsafe {
            let mut sigchld: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut sigchld);
            libc::sigaddset(&mut sigchld, libc::SIGCHLD);
            let mut inherited: libc::sigset_t = std::mem::zeroed();
            let result = libc::pthread_sigmask(libc::SIG_UNBLOCK, &sigchld, &mut inherited);
            if result != 0 {
                return Err(io::Error::from_raw_os_error(result));
            }
            Ok(Self { inherited })
        }
    }
}

#[cfg(unix)]
impl Drop for SigchldUnblockGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = libc::pthread_sigmask(libc::SIG_SETMASK, &self.inherited, std::ptr::null_mut());
        }
    }
}

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

    fn install_sigchld(&self) -> io::Result<()> {
        use std::os::fd::AsRawFd;

        let previous = SIGCHLD_WRITE.swap(self.write.as_raw_fd(), Ordering::AcqRel);
        extern "C" fn on_sigchld(_sig: libc::c_int) {
            write_wake(SIGCHLD_WRITE.load(Ordering::Relaxed));
        }
        unsafe {
            // SAFETY: `action` is a fully initialized sigaction. The handler
            // only writes one byte to a pipe fd published before this call.
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_sigchld as extern "C" fn(libc::c_int) as libc::sighandler_t;
            action.sa_flags = libc::SA_NOCLDSTOP | libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) != 0 {
                let error = io::Error::last_os_error();
                SIGCHLD_WRITE.store(previous, Ordering::Release);
                return Err(error);
            }
        }
        Ok(())
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
    unsafe {
        let errno = errno_location();
        let saved_errno = errno.as_ref().copied();
        let _ = libc::write(fd, byte.as_ptr().cast(), byte.len());
        if let Some(saved_errno) = saved_errno {
            *errno = saved_errno;
        }
    }
}

#[cfg(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "dragonfly"
))]
unsafe fn errno_location() -> *mut libc::c_int {
    unsafe { libc::__error() }
}

#[cfg(any(target_os = "linux", target_os = "hurd", target_os = "redox"))]
unsafe fn errno_location() -> *mut libc::c_int {
    unsafe { libc::__errno_location() }
}

#[cfg(any(
    target_os = "android",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "nuttx"
))]
unsafe fn errno_location() -> *mut libc::c_int {
    unsafe { libc::__errno() }
}

#[cfg(any(target_os = "solaris", target_os = "illumos"))]
unsafe fn errno_location() -> *mut libc::c_int {
    unsafe { libc::___errno() }
}

#[cfg(target_os = "aix")]
unsafe fn errno_location() -> *mut libc::c_int {
    unsafe { libc::_Errno() }
}

// libc exposes no common errno accessor across every Unix target. Unknown
// targets retain an async-signal-safe handler; null disables save/restore
// rather than guessing an ABI symbol.
#[cfg(all(
    unix,
    not(any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "linux",
        target_os = "hurd",
        target_os = "redox",
        target_os = "android",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "nuttx",
        target_os = "solaris",
        target_os = "illumos",
        target_os = "aix"
    ))
))]
unsafe fn errno_location() -> *mut libc::c_int {
    std::ptr::null_mut()
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
        // and children without a waitable handle are still `try_wait`ed after
        // any wake. When every child fits, wait infinitely. When the list is
        // truncated, use a short timeout so omitted exits cannot strand.
        const MAX_WAIT: usize = 64;
        const OVERFLOW_WAIT_MS: u32 = 250;
        let mut handles = [std::ptr::null_mut::<std::ffi::c_void>(); MAX_WAIT];
        handles[0] = self.event;
        let mut count = 1usize;
        let mut truncated = false;
        let mut missing_handle = false;
        for entry in children {
            if count == MAX_WAIT {
                truncated = true;
                break;
            }
            if let Some(handle) = entry.child.as_raw_handle() {
                handles[count] = handle;
                count += 1;
            } else {
                missing_handle = true;
            }
        }
        let timeout = if truncated || missing_handle {
            OVERFLOW_WAIT_MS
        } else {
            INFINITE
        };
        let status = unsafe {
            WaitForMultipleObjects(count as u32, handles.as_ptr() as *const HANDLE, 0, timeout)
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
    fn reaper_thread_startup_failure_is_reported() {
        let result = super::start_reaper_with(|_| {
            Err(std::io::Error::other("injected reaper startup failure"))
        });
        assert!(
            matches!(result, Err(error) if error.to_string() == "injected reaper startup failure")
        );
    }

    #[test]
    fn helper_registration_and_spawn_failures_leave_no_child() {
        let mut command = std::process::Command::new("luvus-nonexistent-helper-test");
        let mut registration_called = false;
        assert!(super::spawn_helper_reaped_with(&mut command, |_| {
            registration_called = true;
            Err(std::io::Error::other("injected disconnected reaper"))
        })
        .is_none());
        assert!(
            registration_called,
            "registration must precede command startup"
        );

        let mut pending = None;
        assert!(super::spawn_helper_reaped_with(&mut command, |child| {
            pending = Some(child);
            Ok(())
        })
        .is_none());
        assert!(super::Registration::Helper(pending.unwrap())
            .into_entry()
            .is_none());
    }

    #[cfg(unix)]
    #[test]
    fn long_running_helpers_do_not_limit_links_or_delay_other_exits() {
        use std::os::fd::OwnedFd;
        use std::os::unix::net::UnixStream;
        use std::process::{Command, Stdio};
        use std::sync::atomic::Ordering;
        use std::time::Instant;

        fn wait_gone(pid: u32) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
                assert!(Instant::now() < deadline, "helper {pid} was not reaped");
                thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }

        let mut writers = Vec::new();
        let mut waiting_pids = Vec::new();
        // More than the old 16-waiter cap, without timing-dependent sleeps in
        // the children. Dropping writers releases them even if an assertion fails.
        for _ in 0..24 {
            let (read, write) = UnixStream::pair().unwrap();
            let mut command = Command::new("sh");
            command
                .args(["-c", "read ignored"])
                .stdin(Stdio::from(OwnedFd::from(read)))
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            waiting_pids.push(super::spawn_helper_reaped(&mut command).expect("no helper cap"));
            writers.push(write);
        }
        assert_eq!(super::CHILD_REAPER_STARTS.load(Ordering::SeqCst), 1);
        for _ in 0..8 {
            let pid = super::spawn_helper_reaped(&mut Command::new("true")).unwrap();
            wait_gone(pid);
        }
        assert!(waiting_pids
            .iter()
            .all(|pid| unsafe { libc::kill(*pid as libc::pid_t, 0) } == 0));
        drop(writers);
        for pid in waiting_pids {
            wait_gone(pid);
        }
        assert_eq!(super::CHILD_REAPER_STARTS.load(Ordering::SeqCst), 1);
    }

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
