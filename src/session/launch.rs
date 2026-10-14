//! Startup-only ownership of a detached server. On Unix the launcher is reaped
//! before success is returned, leaving the server outside the client's tree.
//! Windows retains its direct child and existing detached-process flags.

use std::io;
use std::process::{Child, Command, ExitStatus};

#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::{net::UnixStream, process::ExitStatusExt};
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
pub(super) const SERVER_ROLE: &str = "__server-launch-helper";
#[cfg(not(unix))]
pub(super) const SERVER_ROLE: &str = "server";

pub(super) struct PendingServer {
    child: Child,
    #[cfg(unix)]
    control: UnixStream,
    #[cfg(unix)]
    settled: bool,
}

impl PendingServer {
    pub(super) fn spawn(mut command: Command) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let (control, helper) = UnixStream::pair()?;
            control.set_read_timeout(Some(super::START_TIMEOUT))?;
            control.set_write_timeout(Some(super::START_TIMEOUT))?;
            command.stdin(std::process::Stdio::from(std::os::fd::OwnedFd::from(
                helper,
            )));
            let mut pending = Self {
                child: command.spawn()?,
                control,
                settled: false,
            };
            if pending.reply()? <= 0 {
                return Err(io::Error::other("launcher returned an invalid server PID"));
            }
            Ok(pending)
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                child: command.spawn()?,
            })
        }
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        #[cfg(unix)]
        {
            self.control.write_all(b"P")?;
            let status = self.reply()?;
            Ok((status != -1).then(|| ExitStatus::from_raw(status)))
        }
        #[cfg(not(unix))]
        {
            self.child.try_wait()
        }
    }

    /// Release startup ownership without imposing a deadline on restoration.
    /// Callers own readiness checks; reaping fences reparenting before attach.
    pub(super) fn accept(mut self) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.control.write_all(b"R")?;
            let status = self.reply()?;
            self.settled = true;
            self.reap_launcher()?;
            // A concurrent start may have won the startup lock. Its socket is
            // ready and our losing server may already have exited successfully.
            if status != -1 && !ExitStatus::from_raw(status).success() {
                return Err(io::Error::other(format!(
                    "server exited during startup with {}",
                    ExitStatus::from_raw(status)
                )));
            }
        }
        #[cfg(not(unix))]
        let _ = &mut self;
        Ok(())
    }

    pub(super) fn cancel(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.control.write_all(b"C")?;
            let reply = self.reply();
            self.settled = true;
            let reaped = self.reap_launcher();
            reply.and(reaped)
        }
        #[cfg(not(unix))]
        {
            super::terminate_and_wait(&mut self.child).map_err(io::Error::other)
        }
    }

    #[cfg(unix)]
    fn reply(&mut self) -> io::Result<i32> {
        let mut bytes = [0; 8];
        self.control.read_exact(&mut bytes)?;
        let value = i32::from_le_bytes(bytes[..4].try_into().unwrap());
        let errno = i32::from_le_bytes(bytes[4..].try_into().unwrap());
        if errno != 0 {
            Err(io::Error::from_raw_os_error(errno))
        } else {
            Ok(value)
        }
    }

    #[cfg(unix)]
    fn reap_launcher(&mut self) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = self.child.try_wait()? {
                return if status.success() {
                    Ok(())
                } else {
                    Err(io::Error::other(format!(
                        "server launcher exited with {status}"
                    )))
                };
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "server launcher did not exit",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(unix)]
impl Drop for PendingServer {
    fn drop(&mut self) {
        if !self.settled {
            // Closing the peer also makes the launcher cancel, including when
            // startup failed before its first reply. Never signal an orphan PID.
            let _ = self.control.shutdown(std::net::Shutdown::Both);
            let _ = self.reap_launcher();
        }
    }
}

#[cfg(unix)]
pub(super) fn run_helper() -> io::Result<()> {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::process::Stdio;

    // This private process owns stdin; no other code reads it. Stdio transfers
    // the socket without making extra descriptors inheritable by the server.
    let input = unsafe { OwnedFd::from_raw_fd(libc::STDIN_FILENO) };
    let mut control = UnixStream::from(input);
    control.set_read_timeout(Some(super::START_TIMEOUT + Duration::from_secs(5)))?;
    control.set_write_timeout(Some(super::START_TIMEOUT))?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--session")
        .arg(super::display_name())
        .arg("server")
        .env_remove("LUVUS_SOCKET_PATH")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    super::detach_server_command(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = write_reply(&mut control, Err(error));
            return Err(io::Error::other("could not spawn server"));
        }
    };
    let result = (|| {
        write_reply(&mut control, Ok(child.id() as i32))?;
        loop {
            let mut request = [0];
            control.read_exact(&mut request)?;
            match request[0] {
                b'P' | b'R' => {
                    let status = child
                        .try_wait()
                        .map(|status| status.map(ExitStatusExt::into_raw).unwrap_or(-1));
                    write_reply(&mut control, status)?;
                    if request[0] == b'R' {
                        return Ok(true);
                    }
                }
                b'C' => return Ok(false),
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "invalid launcher request",
                    ))
                }
            }
        }
    })();
    if !matches!(result, Ok(true)) {
        // The actual server remains an owned Child until release. Cancellation
        // and parent EOF can therefore kill/reap it without PID-reuse hazards.
        let cleanup = super::terminate_and_wait(&mut child).map_err(io::Error::other);
        if matches!(result, Ok(false)) {
            write_reply(&mut control, cleanup.map(|_| 0))?;
        }
    }
    result.map(|_| ())
}

#[cfg(unix)]
fn write_reply(control: &mut UnixStream, result: io::Result<i32>) -> io::Result<()> {
    let (value, errno) = match result {
        Ok(value) => (value, 0),
        Err(error) => (0, error.raw_os_error().unwrap_or(libc::EIO)),
    };
    let mut bytes = [0; 8];
    bytes[..4].copy_from_slice(&value.to_le_bytes());
    bytes[4..].copy_from_slice(&errno.to_le_bytes());
    control.write_all(&bytes)
}
