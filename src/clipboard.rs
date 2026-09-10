//! Client-side clipboard helpers. Never borrow a remote server's display environment.

use std::sync::{Arc, Mutex, OnceLock};

/// One bounded completion slot per attachment. Retired attachments reject late
/// completions, so a previous session cannot notify a newly attached client.
pub(crate) struct Completion {
    state: Mutex<(bool, Option<&'static str>)>,
    #[cfg(unix)]
    wake: Option<std::os::unix::net::UnixStream>,
}

impl Completion {
    pub(crate) fn local() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new((true, None)),
            #[cfg(unix)]
            wake: None,
        })
    }

    pub(crate) fn take(&self) -> Option<&'static str> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .1
            .take()
    }

    #[cfg(unix)]
    pub(crate) fn close(&self) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = (false, None);
    }

    pub(crate) fn publish(&self, message: &'static str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.0 {
            return;
        }
        state.1 = Some(message);
        #[cfg(unix)]
        if let Some(mut wake) = self.wake.as_ref() {
            use std::io::Write;
            // Nonblocking, coalesced wakeup. A full socket already signals work.
            loop {
                match wake.write(&[1]) {
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    _ => break,
                }
            }
        }
    }

    fn failure(&self) {
        let language = crate::config::load().language;
        self.publish(crate::i18n::by_code(&language).clipboard_failed);
    }

    #[cfg(unix)]
    pub(crate) fn channel() -> std::io::Result<(Arc<Self>, std::os::unix::net::UnixStream)> {
        let (read, write) = std::os::unix::net::UnixStream::pair()?;
        read.set_nonblocking(true)?;
        write.set_nonblocking(true)?;
        Ok((
            Arc::new(Self {
                state: Mutex::new((true, None)),
                wake: Some(write),
            }),
            read,
        ))
    }
}

fn local_completion() -> Arc<Completion> {
    static LOCAL: OnceLock<Arc<Completion>> = OnceLock::new();
    LOCAL.get_or_init(Completion::local).clone()
}

pub(crate) fn take_notification() -> Option<&'static str> {
    local_completion().take()
}

#[cfg(unix)]
mod native {
    use std::io::{self, Write};
    use std::os::fd::AsRawFd;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    struct Request {
        text: String,
        completion: Arc<super::Completion>,
    }
    type Pending = Arc<(Mutex<Option<Request>>, Condvar)>;

    fn worker(
        queue: &Mutex<Option<Pending>>,
        spawn: impl FnOnce() -> io::Result<Pending>,
    ) -> io::Result<Pending> {
        let mut slot = queue.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pending) = slot.as_ref() {
            return Ok(pending.clone());
        }
        let pending = spawn()?;
        *slot = Some(pending.clone());
        Ok(pending)
    }

    /// One lazy worker, one pending selection. Rapid copies replace pending work
    /// instead of spawning a thread/process for every mouse gesture.
    pub(super) fn copy(text: &str, completion: Arc<super::Completion>) {
        static WORKER: Mutex<Option<Pending>> = Mutex::new(None);
        let worker = worker(&WORKER, || {
            let pending: Pending = Arc::new((Mutex::new(None), Condvar::new()));
            let queue = pending.clone();
            std::thread::Builder::new()
                .name("clipboard".into())
                .spawn(move || loop {
                    let request = {
                        let (lock, ready) = &*queue;
                        let mut slot = lock.lock().unwrap_or_else(|e| e.into_inner());
                        while slot.is_none() {
                            slot = ready.wait(slot).unwrap_or_else(|e| e.into_inner());
                        }
                        slot.take().expect("pending copy")
                    };
                    let helpers = tools();
                    if !helpers.is_empty()
                        && copy_with_tools(&request.text, &helpers, Duration::from_secs(2)).is_err()
                    {
                        // Do not log clipboard contents or helper stderr (either
                        // can contain private data). OSC 52 was already requested.
                        request.completion.failure();
                    }
                })
                .map(|_| pending)
        });
        if let Ok(queue) = worker {
            *queue.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(Request {
                text: text.to_owned(),
                completion,
            });
            queue.1.notify_one();
        } else {
            completion.failure();
        }
    }

    fn tools() -> Vec<(&'static str, &'static [&'static str])> {
        if cfg!(target_os = "macos") {
            return vec![("pbcopy", &[])];
        }
        let mut tools: Vec<(&str, &[&str])> = Vec::new();
        // Preserve the client's actual socket, including wayland-1 and absolute
        // socket paths. Do not guess wayland-0 or rewrite the user's environment.
        if std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty()) {
            tools.push(("wl-copy", &[]));
        }
        if std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty()) {
            tools.push(("xclip", &["-selection", "clipboard"]));
            tools.push(("xsel", &["--clipboard", "--input"]));
        }
        tools
    }

    fn copy_with_tools(text: &str, tools: &[(&str, &[&str])], timeout: Duration) -> io::Result<()> {
        let mut last = io::Error::new(io::ErrorKind::NotFound, "no usable clipboard helper");
        for (program, args) in tools {
            let mut command = Command::new(program);
            command.args(*args);
            match run(&mut command, text.as_bytes(), timeout) {
                Ok(()) => return Ok(()),
                Err(error) => last = error,
            }
        }
        Err(last)
    }

    /// Nonblocking pipe writes and a deadline cover both full stdin pipes and
    /// helpers that consume input but never exit. No extra writer thread can leak.
    fn run(command: &mut Command, bytes: &[u8], timeout: Duration) -> io::Result<()> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let result = (|| {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| io::Error::other("missing clipboard stdin"))?;
            let fd = stdin.as_raw_fd();
            // This pipe is owned exclusively by this worker.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(io::Error::last_os_error());
            }
            let deadline = Instant::now() + timeout;
            let mut remaining = bytes;
            while !remaining.is_empty() {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "clipboard write timed out",
                    ));
                }
                match stdin.write(remaining) {
                    Ok(0) => {
                        return Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "clipboard write failed",
                        ))
                    }
                    Ok(n) => remaining = &remaining[n..],
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(e) => return Err(e),
                }
            }
            drop(stdin); // Helpers require EOF before claiming clipboard ownership.
            loop {
                if let Some(status) = child.try_wait()? {
                    return if status.success() {
                        Ok(())
                    } else {
                        Err(io::Error::other("clipboard helper failed"))
                    };
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "clipboard helper timed out",
                    ));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        })();
        if result.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
        result
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn clipboard_worker_retries_failed_start_and_reuses_success() {
            let slot = Mutex::new(None);
            assert!(worker(&slot, || Err(io::Error::other("spawn failed"))).is_err());
            assert!(slot.lock().unwrap().is_none());
            let pending: Pending = Arc::new((Mutex::new(None), Condvar::new()));
            let started = worker(&slot, || Ok(pending.clone())).unwrap();
            let reused = worker(&slot, || panic!("must reuse the running worker")).unwrap();
            assert!(Arc::ptr_eq(&started, &pending));
            assert!(Arc::ptr_eq(&started, &reused));
        }

        use super::*;

        /// Run explicitly against a disposable compositor, never a user's desktop.
        #[test]
        #[ignore = "requires an isolated Wayland compositor and wl-clipboard"]
        fn clipboard_wayland_round_trip() {
            assert_eq!(
                std::env::var("LUVUS_CLIPBOARD_WAYLAND_TEST").as_deref(),
                Ok("1")
            );
            assert_eq!(std::env::var("WAYLAND_DISPLAY").as_deref(), Ok("wayland-1"));
            let text = "你好\n  clipboard round trip\n";
            let mut failed = Command::new("wl-copy");
            failed.env("WAYLAND_DISPLAY", "luvus-nonexistent-test-socket");
            assert!(run(&mut failed, text.as_bytes(), Duration::from_secs(2)).is_err());
            copy_with_tools(text, &tools(), Duration::from_secs(2)).unwrap();
            let result = Command::new("wl-paste")
                .arg("--no-newline")
                .output()
                .unwrap();
            assert!(result.status.success());
            assert_eq!(result.stdout, text.as_bytes());
            // Also exercise the production worker and latest-pending policy.
            for n in 0..20 {
                copy(&format!("selection {n}"), super::super::local_completion());
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let result = Command::new("wl-paste")
                    .arg("--no-newline")
                    .output()
                    .unwrap();
                if result.status.success() && result.stdout == b"selection 19" {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "latest selection did not reach Wayland"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        #[test]
        fn clipboard_failure_falls_through_to_next_helper() {
            assert!(copy_with_tools(
                "text",
                &[
                    ("/bin/sh", &["-c", "exit 1"]),
                    ("/bin/sh", &["-c", "cat >/dev/null"])
                ],
                Duration::from_secs(1)
            )
            .is_ok());
            assert!(copy_with_tools(
                "text",
                &[("/bin/sh", &["-c", "exit 1"])],
                Duration::from_secs(1)
            )
            .is_err());
        }

        #[test]
        fn clipboard_preserves_unicode_and_newlines() {
            let mut cmd = Command::new("/bin/sh");
            cmd.args([
                "-c",
                "IFS= read -r a; IFS= read -r b; test \"$a\" = '你好' && test \"$b\" = '  second'",
            ]);
            run(
                &mut cmd,
                "你好\n  second\n".as_bytes(),
                Duration::from_secs(1),
            )
            .unwrap();
        }

        #[test]
        fn clipboard_inherits_client_wayland_socket() {
            let mut cmd = Command::new("/bin/sh");
            cmd.env("WAYLAND_DISPLAY", "wayland-1").args([
                "-c",
                "test \"$WAYLAND_DISPLAY\" = wayland-1 && cat >/dev/null",
            ]);
            run(&mut cmd, b"text", Duration::from_secs(1)).unwrap();
        }

        #[test]
        fn clipboard_nonreading_helper_is_bounded() {
            let mut cmd = Command::new("/bin/sh");
            cmd.args(["-c", "exec sleep 10"]);
            let start = Instant::now();
            let error = run(
                &mut cmd,
                &vec![b'x'; 1024 * 1024],
                Duration::from_millis(50),
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            assert!(start.elapsed() < Duration::from_secs(2));
        }

        #[test]
        fn clipboard_waiting_helper_is_bounded() {
            let mut cmd = Command::new("/bin/sh");
            cmd.args(["-c", "cat >/dev/null; exec sleep 10"]);
            assert_eq!(
                run(&mut cmd, b"text", Duration::from_millis(50))
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::TimedOut
            );
        }
    }
}

pub(crate) fn copy_native(text: &str) {
    copy_native_to(text, local_completion());
}

pub(crate) fn copy_native_to(text: &str, completion: Arc<Completion>) {
    #[cfg(unix)]
    native::copy(text, completion);
    #[cfg(not(unix))]
    if crate::system_clipboard_copy(text).is_err() {
        completion.failure();
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn clipboard_retired_completion_rejects_late_results() {
        let (completion, _wake) = super::Completion::channel().unwrap();
        completion.publish("before detach");
        completion.close();
        completion.publish("after detach");
        assert_eq!(completion.take(), None);
    }

    #[test]
    fn clipboard_completion_is_coalesced_and_consumed_once() {
        let completion = super::Completion::local();
        completion.publish("first");
        completion.publish("latest");
        assert_eq!(completion.take(), Some("latest"));
        assert_eq!(completion.take(), None);
    }
}
