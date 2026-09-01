//! Cross-platform local IPC. Unix-domain sockets on Unix, named pipes on
//! Windows, behind one cloneable read+write `Conn` so the client/server stay
//! portable (replaces the previous `std::os::unix::net` usage). The socket is
//! still identified by a per-session filesystem path; on Windows that path is
//! hashed into a named-pipe id (pipes aren't filesystem paths).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
#[cfg(windows)]
use std::time::Instant;

use fs2::FileExt;
use interprocess::local_socket::prelude::*;
#[cfg(not(windows))]
use interprocess::local_socket::ConnectOptions;
use interprocess::local_socket::{ListenerOptions, Stream};
#[cfg(not(windows))]
use interprocess::ConnectWaitMode;

pub use interprocess::local_socket::Listener;

/// Exclusive, process-scoped guard for creating Luvus's two server sockets.
///
/// The lock file remains on disk after the holder exits; the OS releases the
/// advisory lock automatically, including after a crash. Keeping the file
/// avoids a second race around creating and deleting a lock pathname.
pub struct ServerStartupLock {
    _file: File,
}

/// Acquire exclusive ownership of server startup for one Luvus state directory.
/// Hold the returned guard while checking, reclaiming, and binding both sockets.
pub fn acquire_server_startup_lock(state_dir: &Path) -> io::Result<ServerStartupLock> {
    fs::create_dir_all(state_dir)?;
    let path = state_dir.join("server.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    file.lock_exclusive()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(ServerStartupLock { _file: file })
}

impl ServerStartupLock {
    /// Remove a socket only after its listener is proven unreachable. A live
    /// socket is never replaced: doing so would orphan its server process.
    pub fn reclaim_stale_socket(&self, path: &Path) -> io::Result<()> {
        #[cfg(windows)]
        {
            let _ = path;
            Ok(())
        }
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::FileTypeExt;

            let metadata = match fs::symlink_metadata(path) {
                Ok(metadata) => metadata,
                Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(err) => return Err(err),
            };
            if !metadata.file_type().is_socket() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("refusing to replace non-socket path {}", path.display()),
                ));
            }
            if connect_for_liveness(path).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("a Luvus listener is already active at {}", path.display()),
                ));
            }
            fs::remove_file(path)
        }
    }
}

#[cfg(not(windows))]
fn connect_for_liveness(path: &Path) -> io::Result<Conn> {
    use interprocess::local_socket::GenericFilePath;
    let name = path.to_fs_name::<GenericFilePath>()?;
    Ok(Conn::new(Stream::connect(name)?))
}

/// A cloneable owned read+write handle to one connection — the portable
/// stand-in for a cloned `UnixStream`. Clones share the full-duplex socket, so
/// one clone can read while another writes (as `try_clone` did before).
#[derive(Clone)]
pub struct Conn(Arc<Stream>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimeoutMode {
    Kernel,
    Nonblocking,
}

impl Conn {
    fn new(stream: Stream) -> Self {
        Conn(Arc::new(stream))
    }

    /// Bound control-plane requests such as cross-session search. Unix local
    /// sockets support kernel timeouts; named pipes may report Unsupported, in
    /// which case the connection becomes nonblocking so the caller can enforce
    /// an application deadline without leaving a blocked worker behind.
    pub fn set_timeouts(&self, timeout: Duration) -> io::Result<TimeoutMode> {
        let receive = self.set_recv_timeout(timeout)?;
        let send = self.set_send_timeout(timeout)?;
        Ok(
            if receive == TimeoutMode::Nonblocking || send == TimeoutMode::Nonblocking {
                TimeoutMode::Nonblocking
            } else {
                TimeoutMode::Kernel
            },
        )
    }

    pub fn set_recv_timeout(&self, timeout: Duration) -> io::Result<TimeoutMode> {
        use interprocess::local_socket::traits::Stream as _;
        match self.0.set_recv_timeout(Some(timeout)) {
            Ok(()) => Ok(TimeoutMode::Kernel),
            Err(_) => {
                // Named pipes reject kernel timeouts. PIPE_NOWAIT on an
                // overlapped client handle also fails after a write, so Windows
                // deadline readers poll with PeekNamedPipe instead of changing mode.
                #[cfg(windows)]
                {
                    let _ = timeout;
                    Ok(TimeoutMode::Nonblocking)
                }
                #[cfg(not(windows))]
                {
                    self.0.set_nonblocking(true)?;
                    Ok(TimeoutMode::Nonblocking)
                }
            }
        }
    }

    pub fn set_send_timeout(&self, timeout: Duration) -> io::Result<TimeoutMode> {
        use interprocess::local_socket::traits::Stream as _;
        match self.0.set_send_timeout(Some(timeout)) {
            Ok(()) => Ok(TimeoutMode::Kernel),
            Err(error) if error.kind() == io::ErrorKind::Unsupported => {
                self.0.set_nonblocking(true)?;
                Ok(TimeoutMode::Nonblocking)
            }
            Err(error) => Err(error),
        }
    }

    /// Restore blocking I/O after a named-pipe timeout fallback. Unix kernel
    /// timeouts do not change this mode, so callers use this only when
    /// `set_timeouts` returned [`TimeoutMode::Nonblocking`].
    pub fn set_blocking(&self) -> io::Result<()> {
        use interprocess::local_socket::traits::Stream as _;
        self.0.set_nonblocking(false)
    }

    /// True when a Windows deadline reader can issue a blocking read without
    /// waiting. `ERROR_NO_DATA` / not-yet-accepted pipes count as empty.
    ///
    /// Do not switch the pipe to `PIPE_NOWAIT` after a write: that
    /// `SetNamedPipeHandleState` call returns `ERROR_PIPE_BUSY`.
    #[cfg(windows)]
    pub fn recv_has_data(&self) -> io::Result<bool> {
        use std::os::windows::io::{AsHandle, AsRawHandle};
        use windows_sys::Win32::Foundation::{
            ERROR_NO_DATA, ERROR_PIPE_LISTENING, ERROR_PIPE_NOT_CONNECTED,
        };
        use windows_sys::Win32::System::Pipes::PeekNamedPipe;

        let Stream::NamedPipe(pipe) = &*self.0;
        let mut available = 0u32;
        let ok = unsafe {
            PeekNamedPipe(
                pipe.inner().as_handle().as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        };
        if ok != 0 {
            return Ok(available > 0);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(code)
                if code == ERROR_NO_DATA as i32
                    || code == ERROR_PIPE_NOT_CONNECTED as i32
                    || code == ERROR_PIPE_LISTENING as i32 =>
            {
                Ok(false)
            }
            _ => Err(error),
        }
    }

    /// PID of the process that owns the listening endpoint.
    ///
    /// Used by `server stop` when the app loop no longer answers: the pipe or
    /// socket can still accept connections while requests hang forever.
    pub fn server_pid(&self) -> io::Result<u32> {
        #[cfg(windows)]
        {
            let Stream::NamedPipe(pipe) = &*self.0;
            pipe.inner().server_process_id()
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            use std::mem::{size_of, zeroed};
            use std::os::fd::AsRawFd;

            let Stream::UdSocket(socket) = &*self.0;
            let mut credentials: libc::ucred = unsafe { zeroed() };
            let mut len = size_of::<libc::ucred>() as libc::socklen_t;
            let result = unsafe {
                libc::getsockopt(
                    socket.inner().as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&raw mut credentials).cast(),
                    &raw mut len,
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(credentials.pid as u32)
        }
        #[cfg(not(any(windows, target_os = "linux", target_os = "android")))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "server pid is not available on this transport",
            ))
        }
    }
}

#[cfg(unix)]
fn current_euid() -> u32 {
    // SAFETY: `geteuid` has no preconditions and does not dereference memory.
    unsafe { libc::geteuid() }
}

#[cfg(unix)]
fn validate_unix_socket_path(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing non-socket Luvus endpoint {}", path.display()),
        ));
    }
    if metadata.uid() != current_euid() || metadata.mode() & 0o777 != 0o600 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing Luvus socket without current-user ownership and mode 0600: {}",
                path.display()
            ),
        ));
    }
    Ok(())
}

/// Verify that the connected local peer belongs to the current account.
/// Filesystem permissions are necessary but not sufficient once a descriptor
/// has been accepted, so Unix checks peer credentials just as Windows checks
/// the named-pipe server process owner.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn validate_peer(conn: &Conn) -> io::Result<()> {
    use std::mem::{size_of, zeroed};
    use std::os::fd::AsRawFd;

    let Stream::UdSocket(socket) = &*conn.0;
    // SAFETY: the kernel writes at most `size_of::<ucred>()` bytes into a valid
    // stack value, and `len` accurately describes that storage.
    let mut credentials: libc::ucred = unsafe { zeroed() };
    let mut len = size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            socket.inner().as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut credentials).cast(),
            &raw mut len,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if credentials.uid != current_euid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing a Luvus socket peer owned by another account",
        ));
    }
    Ok(())
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
pub fn validate_peer(conn: &Conn) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    let Stream::UdSocket(socket) = &*conn.0;
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: both output pointers reference initialized stack storage and the
    // descriptor belongs to a connected Unix-domain socket.
    let result =
        unsafe { libc::getpeereid(socket.inner().as_raw_fd(), &raw mut uid, &raw mut gid) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if uid != current_euid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing a Luvus socket peer owned by another account",
        ));
    }
    Ok(())
}

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))
))]
pub fn validate_peer(_conn: &Conn) -> io::Result<()> {
    Ok(())
}

#[cfg(windows)]
pub fn validate_peer(_conn: &Conn) -> io::Result<()> {
    // The private DACL limits clients to the owner/System. The client side also
    // verifies the connected server process belongs to the current account.
    Ok(())
}

/// Whether a nonblocking local-socket read should be retried. Windows reports
/// `ERROR_NO_DATA` for a connected PIPE_NOWAIT stream with no input available,
/// and Rust maps that code to `BrokenPipe` rather than `WouldBlock`.
pub fn nonblocking_read_pending(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::WouldBlock {
        return true;
    }
    #[cfg(windows)]
    {
        error.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_NO_DATA as i32)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Windows byte-mode named pipes can return a successful zero-byte read while
/// PIPE_NOWAIT has no data. Unix stream sockets reserve zero for peer EOF.
pub const fn nonblocking_zero_is_pending() -> bool {
    cfg!(windows)
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self.0).read(buf)
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&*self.0).write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        #[cfg(windows)]
        {
            // `interprocess` intentionally makes `Write::flush` on its local
            // socket wrapper a no-op on Windows. Reach the named-pipe stream
            // so one-shot responses are delivered before this connection is
            // dropped instead of relying on the crate's deferred drop flush.
            let Stream::NamedPipe(pipe) = &*self.0;
            pipe.inner().flush()
        }
        #[cfg(not(windows))]
        {
            (&*self.0).flush()
        }
    }
}

#[cfg(windows)]
fn pipe_id(path: &Path) -> String {
    namespaced_pipe_id(path, "luvus")
}

#[cfg(windows)]
fn namespaced_pipe_id(path: &Path, namespace: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    format!("{namespace}-{:016x}", h.finish())
}

#[cfg(windows)]
fn private_pipe_security_descriptor(
) -> io::Result<interprocess::os::windows::security_descriptor::SecurityDescriptor> {
    use interprocess::os::windows::security_descriptor::SecurityDescriptor;
    use widestring::U16CString;

    // Protected DACL: full access only to LocalSystem and the pipe object owner
    // (the account running this Luvus server). `interprocess` creates local-only
    // named pipes by default, adding PIPE_REJECT_REMOTE_CLIENTS independently.
    let sddl = U16CString::from_str("D:P(A;;GA;;;SY)(A;;GA;;;OW)")
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    SecurityDescriptor::deserialize(&sddl)
}

#[cfg(windows)]
fn validate_connected_server(stream: &Stream) -> io::Result<()> {
    let Stream::NamedPipe(pipe) = stream;
    let server_pid = pipe.inner().server_process_id()?;
    if !crate::platform::process_belongs_to_current_user(server_pid) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing a Luvus named pipe owned by another Windows account",
        ));
    }
    Ok(())
}

/// Return the actual address a protocol consumer passes to its platform
/// transport. Unix uses the socket path; Windows exposes the namespaced-pipe
/// identifier instead of asking consumers to reproduce our hash.
pub(crate) fn discovery_address(path: &Path) -> String {
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\{}", pipe_id(path))
    }
    #[cfg(not(windows))]
    {
        path.display().to_string()
    }
}

/// Connect, but do not block the caller forever.
///
/// Windows named-pipe `CreateFile` waits indefinitely when every instance is
/// busy. Loop `CreateFile` / `WaitNamedPipe` against the remaining deadline
/// instead of calling unbounded `connect()` after one successful wait.
pub fn connect_timeout(path: &Path, timeout: Duration) -> io::Result<Conn> {
    #[cfg(windows)]
    {
        connect_named_pipe_timeout(path, timeout)
    }
    #[cfg(not(windows))]
    {
        use interprocess::local_socket::GenericFilePath;
        validate_unix_socket_path(path)?;
        let name = path.to_fs_name::<GenericFilePath>()?;
        let stream = ConnectOptions::new()
            .name(name)
            .wait_mode(ConnectWaitMode::Timeout(timeout))
            .connect_sync()
            .map_err(|error| {
                if error.kind() == io::ErrorKind::WouldBlock {
                    io::Error::new(io::ErrorKind::TimedOut, error)
                } else {
                    error
                }
            })?;
        let conn = Conn::new(stream);
        validate_peer(&conn)?;
        Ok(conn)
    }
}

/// True when the endpoint exists: we connected, or the wait timed out because
/// it is busy/hung. False when the name is absent.
pub fn endpoint_exists(path: &Path, timeout: Duration) -> bool {
    match connect_timeout(path, timeout) {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::TimedOut => true,
        Err(_) => false,
    }
}

#[cfg(windows)]
fn connect_named_pipe_timeout(path: &Path, timeout: Duration) -> io::Result<Conn> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{
        ERROR_PIPE_BUSY, ERROR_SEM_TIMEOUT, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Pipes::WaitNamedPipeW;

    let name = format!(r"\\.\pipe\{}", pipe_id(path));
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let deadline = Instant::now() + timeout;
    loop {
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            let owned = unsafe { OwnedHandle::from_raw_handle(handle) };
            let np = interprocess::os::windows::named_pipe::local_socket::Stream::try_from(owned)
                .map_err(|error| io::Error::other(error.to_string()))?;
            let stream = Stream::from(np);
            validate_connected_server(&stream)?;
            return Ok(Conn::new(stream));
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_PIPE_BUSY as i32) {
            return Err(error);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "connection timed out",
            ));
        }
        let ms = u32::try_from(remaining.as_millis()).unwrap_or(u32::MAX);
        let waited = unsafe { WaitNamedPipeW(wide.as_ptr(), ms) };
        if waited != 0 {
            continue;
        }
        let wait_error = io::Error::last_os_error();
        if wait_error.raw_os_error() == Some(ERROR_SEM_TIMEOUT as i32) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "connection timed out",
            ));
        }
        return Err(wait_error);
    }
}

/// Connect to a server socket identified by a per-session filesystem path.
pub fn connect(path: &Path) -> io::Result<Conn> {
    #[cfg(windows)]
    {
        use interprocess::local_socket::GenericNamespaced;
        let id = pipe_id(path);
        let name = id.to_ns_name::<GenericNamespaced>()?;
        let stream = Stream::connect(name)?;
        validate_connected_server(&stream)?;
        Ok(Conn::new(stream))
    }
    #[cfg(not(windows))]
    {
        use interprocess::local_socket::GenericFilePath;
        validate_unix_socket_path(path)?;
        let name = path.to_fs_name::<GenericFilePath>()?;
        let conn = Conn::new(Stream::connect(name)?);
        validate_peer(&conn)?;
        Ok(conn)
    }
}

/// Bind a listener at the given per-session path.
///
/// Call [`ServerStartupLock::reclaim_stale_socket`] first while holding the
/// state-directory startup lock. This function never removes an existing path.
pub fn bind(path: &Path) -> io::Result<Listener> {
    #[cfg(windows)]
    {
        use interprocess::local_socket::GenericNamespaced;
        use interprocess::os::windows::local_socket::ListenerOptionsExt as _;
        let id = pipe_id(path);
        let name = id.to_ns_name::<GenericNamespaced>()?;
        ListenerOptions::new()
            .name(name)
            .reclaim_name(false)
            .security_descriptor(private_pipe_security_descriptor()?)
            .create_sync()
    }
    #[cfg(not(windows))]
    {
        use interprocess::local_socket::GenericFilePath;
        let name = path.to_fs_name::<GenericFilePath>()?;
        let listener = ListenerOptions::new().name(name).create_sync()?;
        // Owner-only: a connection to this socket is full command execution as
        // the user, so never rely on the umask (the selected session dir is also
        // forced to 0700 — see `persist::ensure_session_dir`).
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(error) =
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            {
                drop(listener);
                let _ = std::fs::remove_file(path);
                return Err(error);
            }
        }
        Ok(listener)
    }
}

/// Iterate accepted connections (errors skipped), as `Conn`s.
pub fn incoming(listener: &Listener) -> impl Iterator<Item = Conn> + '_ {
    listener.incoming().flatten().map(Conn::new)
}

#[cfg(all(test, windows))]
mod windows_security_tests {
    use super::*;

    fn test_pipe(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("luvus-{name}-{}", std::process::id()))
    }

    #[test]
    fn private_named_pipe_accepts_the_current_user() {
        let path = test_pipe("private-pipe");
        let listener = bind(&path).expect("bind owner-only named pipe");
        let client_path = path.clone();
        let client = std::thread::spawn(move || {
            let mut client = connect(&client_path).expect("same-user client connects");
            client.write_all(b"luvus").unwrap();
        });
        let mut server = listener.accept().expect("accept same-user client");
        let mut bytes = [0_u8; 5];
        server.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"luvus");
        client.join().unwrap();
    }

    #[test]
    fn discovery_exposes_the_real_windows_pipe_address() {
        let address = discovery_address(&test_pipe("discovery"));
        assert!(address.starts_with(r"\\.\pipe\luvus-"));
        assert!(!address.ends_with(".sock"));
    }

    #[test]
    fn pipe_nowait_after_write_fails_on_a_fresh_pipe() {
        use interprocess::local_socket::traits::Stream as _;

        let path = test_pipe("nowait-after-write");
        let listener = bind(&path).expect("bind owner-only named pipe");
        let client_path = path.clone();
        let client = std::thread::spawn(move || {
            let mut conn = connect(&client_path).expect("same-user client connects");
            let before = conn.0.set_nonblocking(true);
            let _ = conn.0.set_nonblocking(false);
            writeln!(conn, "x").unwrap();
            let after = conn.0.set_nonblocking(true);
            (before, after)
        });
        let _server = listener.accept().expect("accept same-user client");
        let (before, after) = client.join().unwrap();
        before.expect("PIPE_NOWAIT before write is allowed");
        let error = after.expect_err("PIPE_NOWAIT after write must fail on a fresh pipe");
        assert_eq!(
            error.raw_os_error(),
            Some(windows_sys::Win32::Foundation::ERROR_PIPE_BUSY as i32),
            "expected ERROR_PIPE_BUSY after write, got {error}"
        );
    }

    #[test]
    fn client_sees_the_named_pipe_server_pid() {
        let path = test_pipe("server-pid");
        let listener = bind(&path).expect("bind owner-only named pipe");
        let client_path = path.clone();
        let expected = std::process::id();
        let client = std::thread::spawn(move || {
            let conn = connect(&client_path).expect("same-user client connects");
            conn.server_pid().expect("named-pipe server pid")
        });
        let _server = listener.accept().expect("accept same-user client");
        assert_eq!(client.join().unwrap(), expected);
    }

    #[test]
    fn connect_timeout_fails_fast_when_the_pipe_is_absent() {
        match connect_timeout(&test_pipe("missing"), Duration::from_secs(1)) {
            Ok(_) => panic!("absent pipe must not connect"),
            Err(err) => assert_ne!(err.kind(), io::ErrorKind::TimedOut),
        }
    }

    #[test]
    fn connect_timeout_returns_quickly_when_the_pipe_is_listening() {
        let path = test_pipe("listening-no-accept");
        let _listener = bind(&path).expect("bind owner-only named pipe");
        let started = std::time::Instant::now();
        let result = connect_timeout(&path, Duration::from_millis(200));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "connect_timeout must not pin the process on a listening pipe"
        );
        match result {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {}
            Err(error) => panic!("unexpected connect_timeout error: {error}"),
        }
    }

    #[test]
    fn connect_timeout_times_out_when_every_instance_is_busy() {
        let path = test_pipe("busy-timeout");
        let _listener = bind(&path).expect("bind owner-only named pipe");
        let first = connect_timeout(&path, Duration::from_millis(200))
            .expect("first client occupies the free instance");
        let started = std::time::Instant::now();
        let result = connect_timeout(&path, Duration::from_millis(200));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "busy-pipe connect_timeout must not call unbounded connect"
        );
        match result {
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {}
            Ok(_) => panic!("busy pipe must time out, got a connection"),
            Err(error) => panic!("busy pipe must time out, got {error}"),
        }
        drop(first);
    }

    #[test]
    fn busy_named_pipe_is_not_treated_as_absent() {
        let path = test_pipe("busy-exists");
        let _listener = bind(&path).expect("bind owner-only named pipe");
        assert!(
            endpoint_exists(&path, Duration::from_millis(200)),
            "a listening pipe must count as present even if no instance is free"
        );
        assert!(!endpoint_exists(
            &test_pipe("no-such-pipe"),
            Duration::from_millis(200)
        ));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{acquire_server_startup_lock, ServerStartupLock};
    use std::io;
    use std::os::unix::net::UnixListener;

    fn test_socket(
        name: &str,
    ) -> (
        crate::persist::TestEnv,
        ServerStartupLock,
        std::path::PathBuf,
    ) {
        let env = crate::persist::test_env(name);
        let dir = crate::persist::ensure_config_dir();
        let lock = acquire_server_startup_lock(&dir).unwrap();
        (env, lock, dir.join("luvus.sock"))
    }

    #[test]
    fn live_socket_is_never_reclaimed() {
        let (_env, lock, path) = test_socket("live-socket");
        let _listener = UnixListener::bind(&path).unwrap();

        let err = lock.reclaim_stale_socket(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        assert!(path.exists(), "a live socket pathname must remain in place");
    }

    #[test]
    fn stale_socket_is_reclaimed_while_holding_startup_lock() {
        let (_env, lock, path) = test_socket("stale-socket");
        let listener = UnixListener::bind(&path).unwrap();
        drop(listener);
        assert!(path.exists(), "dropping a UnixListener leaves a stale path");

        lock.reclaim_stale_socket(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn non_socket_path_is_not_deleted() {
        let (_env, lock, path) = test_socket("non-socket");
        std::fs::write(&path, "do not delete").unwrap();

        let err = lock.reclaim_stale_socket(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "do not delete");
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::{nonblocking_read_pending, pipe_id};
    use std::io;
    use std::path::Path;

    #[test]
    fn named_session_paths_derive_distinct_stable_pipe_ids() {
        let alpha = pipe_id(Path::new(r"C:\Users\riz\.luvus\sessions\alpha\luvus.sock"));
        let beta = pipe_id(Path::new(r"C:\Users\riz\.luvus\sessions\beta\luvus.sock"));
        assert_ne!(alpha, beta);
        assert_eq!(
            alpha,
            pipe_id(Path::new(r"C:\Users\riz\.luvus\sessions\alpha\luvus.sock"))
        );
    }

    #[test]
    fn pipe_nowait_no_data_is_retryable_despite_broken_pipe_kind() {
        let error =
            io::Error::from_raw_os_error(windows_sys::Win32::Foundation::ERROR_NO_DATA as i32);
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(nonblocking_read_pending(&error));
    }
}
