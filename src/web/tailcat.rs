use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

const SUPPORTED_VERSION: &str = "v0.2.0";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_OUTPUT_BYTES: usize = 16 * 1024;

pub(super) struct Tailcat {
    child: Child,
    address: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StartFailure {
    Missing,
    Unsupported,
    Failed,
}

impl Tailcat {
    pub(super) fn start(port: u16) -> Result<Self, StartFailure> {
        verify_version("tailcat")?;
        Self::start_program("tailcat", port).map_err(|_| StartFailure::Failed)
    }

    fn start_program(program: &str, port: u16) -> Result<Self> {
        let mut command = Command::new(program);
        configure_server_command(&mut command, port);
        let mut child = command
            .spawn()
            .with_context(|| format!("cannot start supported Tailcat {SUPPORTED_VERSION}"))?;
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                terminate_child(&mut child);
                return Err(anyhow!("Tailcat startup output is unavailable"));
            }
        };
        let (sender, receiver) = mpsc::sync_channel(1);
        let reader = match thread::Builder::new()
            .name("luvus-tailcat-startup".into())
            .stack_size(128 * 1024)
            .spawn(move || {
                let _ = sender.send(read_line_bounded(stdout, MAX_OUTPUT_BYTES));
            }) {
            Ok(reader) => reader,
            Err(error) => {
                terminate_child(&mut child);
                return Err(error).context("cannot read Tailcat startup output");
            }
        };

        let deadline = Instant::now() + STARTUP_TIMEOUT;
        let line = loop {
            match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(result) => break result,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "Tailcat startup output closed",
                    ));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(status) = child.try_wait()? {
                        break Err(io::Error::other(format!(
                            "Tailcat exited during startup ({status})"
                        )));
                    }
                    if Instant::now() >= deadline {
                        break Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "Tailcat startup timed out",
                        ));
                    }
                }
            }
        };
        if line.is_err() {
            terminate_child(&mut child);
        }
        let _ = reader.join();
        let line = line?;
        let address = match parse_startup_json(&line) {
            Ok(address) => address,
            Err(error) => {
                terminate_child(&mut child);
                return Err(error);
            }
        };
        Ok(Self { child, address })
    }

    pub(super) fn address(&self) -> &str {
        &self.address
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    pub(super) fn stop(&mut self) {
        terminate_child(&mut self.child);
    }
}

impl Drop for Tailcat {
    fn drop(&mut self) {
        self.stop();
    }
}

fn verify_version(program: &str) -> Result<(), StartFailure> {
    let output = run_bounded(program, &["--version"], COMMAND_TIMEOUT, MAX_OUTPUT_BYTES).map_err(
        |error| {
            if error.kind() == io::ErrorKind::NotFound {
                StartFailure::Missing
            } else {
                StartFailure::Failed
            }
        },
    )?;
    let version = String::from_utf8(output).map_err(|_| StartFailure::Failed)?;
    if version.trim() != SUPPORTED_VERSION {
        return Err(StartFailure::Unsupported);
    }
    Ok(())
}

fn configure_server_command(command: &mut Command, port: u16) {
    command
        .arg("--key=new")
        .arg("--json")
        // Embed the chosen DERP relay in the address so the standalone static
        // browser client does not need a credential-adjacent map proxy.
        .arg("--full-address")
        .arg(format!("--serve={port}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    crate::platform::no_window(command);
}

fn parse_startup_json(line: &[u8]) -> Result<String> {
    let value: Value = serde_json::from_slice(line).context("invalid Tailcat startup JSON")?;
    let valid_shape = value
        .as_object()
        .is_some_and(|object| object.len() == 1 && object.contains_key("listenAddr"));
    let address = value
        .get("listenAddr")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !valid_shape
        || !address.starts_with("tc")
        || address.len() > 12 * 1024
        || !address.is_ascii()
    {
        return Err(anyhow!("invalid Tailcat connection address"));
    }
    Ok(address.to_string())
}

fn run_bounded(
    program: &str,
    args: &[&str],
    timeout: Duration,
    limit: usize,
) -> io::Result<Vec<u8>> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    crate::platform::no_window(&mut command);
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("child stdout unavailable"))?;
    let (sender, receiver) = mpsc::sync_channel(1);
    let reader = thread::Builder::new()
        .name("luvus-tailcat-version".into())
        .stack_size(128 * 1024)
        .spawn(move || {
            let _ = sender.send(read_all_bounded(stdout, limit));
        })?;
    let deadline = Instant::now() + timeout;
    let result = loop {
        match receiver.recv_timeout(Duration::from_millis(25)) {
            Ok(result) => break result,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "child output closed",
                ));
            }
            Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() >= deadline => {
                terminate_child(&mut child);
                break Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "child command timed out",
                ));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            terminate_child(&mut child);
            let _ = reader.join();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "child command did not exit",
            ));
        }
        thread::sleep(Duration::from_millis(10));
    };
    let _ = reader.join();
    if !status.success() {
        return Err(io::Error::other("child command failed"));
    }
    result
}

fn read_line_bounded(mut reader: impl Read, limit: usize) -> io::Result<Vec<u8>> {
    let mut output = Vec::with_capacity(256);
    let mut byte = [0_u8; 1];
    loop {
        match reader.read(&mut byte)? {
            0 if output.is_empty() => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "startup output is empty",
                ));
            }
            0 => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "startup output is missing LF",
                ));
            }
            _ if byte[0] == b'\n' => return Ok(output),
            _ if output.len() >= limit => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "startup output is too large",
                ));
            }
            _ => output.push(byte[0]),
        }
    }
}

fn read_all_bounded(mut reader: impl Read, limit: usize) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    reader
        .by_ref()
        .take((limit + 1) as u64)
        .read_to_end(&mut output)?;
    if output.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "child output is too large",
        ));
    }
    Ok(output)
}

fn terminate_child(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_json_is_strict_and_bounded() {
        assert_eq!(
            parse_startup_json(br#"{"listenAddr":"tcABC123"}"#).unwrap(),
            "tcABC123"
        );
        assert!(parse_startup_json(br#"{"listenAddr":"http://wrong"}"#).is_err());
        assert!(parse_startup_json(br#"{"listenAddr":"tcABC","extra":1}"#).is_err());
    }

    #[test]
    fn server_command_is_ephemeral_and_serves_only_one_port() {
        let mut command = Command::new("tailcat");
        configure_server_command(&mut command, 43210);
        let debug = format!("{command:?}");
        assert!(debug.contains("\"--key=new\""));
        assert!(debug.contains("\"--json\""));
        assert!(debug.contains("\"--full-address\""));
        assert!(debug.contains("\"--serve=43210\""));
        assert!(!debug.contains("--serve=all"));
    }

    #[test]
    fn line_reader_rejects_missing_newline_and_oversize() {
        assert_eq!(read_line_bounded(&b"ok\nrest"[..], 8).unwrap(), b"ok");
        assert!(read_line_bounded(&b"no-newline"[..], 32).is_err());
        assert!(read_line_bounded(&b"too-long\n"[..], 3).is_err());
    }

    #[test]
    fn missing_tailcat_is_classified_without_exposing_process_details() {
        assert_eq!(
            verify_version("definitely-not-a-real-tailcat-binary"),
            Err(StartFailure::Missing)
        );
    }
}
