//! Explicit, bounded Unix clipboard-image access.
//!
//! The helpers run only for an exact image-paste gesture in the display client.
//! They inherit that client's display environment, never the detached server's.

use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::clipboard_image::{validate_png, MAX_PNG_BYTES};

const HELPER_TIMEOUT: Duration = Duration::from_millis(750);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Reader {
    program: &'static str,
    args: &'static [&'static str],
}

#[cfg(target_os = "macos")]
const MACOS_PASTEBOARD_SCRIPT: &str = r#"
ObjC.import('AppKit');
ObjC.import('Foundation');
ObjC.import('stdlib');

const pasteboard = $.NSPasteboard.generalPasteboard;
let data = pasteboard.dataForType('public.png');
if (data.isNil()) {
    const tiff = pasteboard.dataForType('public.tiff');
    if (tiff.isNil() || Number(tiff.length) > 50331648) $.exit(2);
    const bitmap = $.NSBitmapImageRep.imageRepWithData(tiff);
    if (bitmap.isNil()) $.exit(2);
    const width = Number(bitmap.pixelsWide);
    const height = Number(bitmap.pixelsHigh);
    if (width < 1 || height < 1 || width > 8192 || height > 8192 || width * height > 12000000) {
        $.exit(3);
    }
    data = bitmap.representationUsingTypeProperties($.NSBitmapImageFileTypePNG, $({}));
}
if (data.isNil() || Number(data.length) > 50331648) $.exit(3);
$.NSFileHandle.fileHandleWithStandardOutput.writeData(data);
$.exit(0);
"#;

pub(super) fn clipboard_image() -> Option<Vec<u8>> {
    for reader in readers() {
        let mut command = Command::new(reader.program);
        command.args(reader.args);
        let Ok(bytes) = read_bounded(&mut command, HELPER_TIMEOUT) else {
            continue;
        };
        if validate_png(&bytes).is_ok() {
            return Some(bytes);
        }
    }
    None
}

fn readers() -> Vec<Reader> {
    #[cfg(target_os = "macos")]
    {
        vec![Reader {
            program: "/usr/bin/osascript",
            args: &["-l", "JavaScript", "-e", MACOS_PASTEBOARD_SCRIPT],
        }]
    }
    #[cfg(not(target_os = "macos"))]
    {
        graphical_readers(
            std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty()),
            std::env::var_os("DISPLAY").is_some_and(|value| !value.is_empty()),
        )
    }
}

#[cfg(any(not(target_os = "macos"), test))]
fn graphical_readers(wayland: bool, x11: bool) -> Vec<Reader> {
    let mut readers = Vec::with_capacity(2);
    if wayland {
        readers.push(Reader {
            program: "wl-paste",
            args: &["--no-newline", "--type", "image/png"],
        });
    }
    if x11 {
        readers.push(Reader {
            program: "xclip",
            args: &["-selection", "clipboard", "-target", "image/png", "-out"],
        });
    }
    readers
}

/// Drain helper output without allowing an unavailable clipboard owner or a
/// malicious helper to block input forever or allocate without a fixed bound.
fn read_bounded(command: &mut Command, timeout: Duration) -> io::Result<Vec<u8>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let result = (|| {
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("missing clipboard stdout"))?;
        let fd = stdout.as_raw_fd();
        // This descriptor belongs exclusively to this short-lived reader.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }

        let deadline = Instant::now() + timeout;
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 16 * 1024];
        let mut eof = false;
        let mut status = None;
        loop {
            let mut progressed = false;
            if !eof {
                match stdout.read(&mut chunk) {
                    Ok(0) => eof = true,
                    Ok(count) => {
                        if bytes.len().saturating_add(count) > MAX_PNG_BYTES {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "clipboard image exceeds size limit",
                            ));
                        }
                        bytes.extend_from_slice(&chunk[..count]);
                        progressed = true;
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) => return Err(error),
                }
            }
            if status.is_none() {
                status = child.try_wait()?;
            }
            if eof {
                if let Some(status) = status {
                    return if status.success() {
                        Ok(bytes)
                    } else {
                        Err(io::Error::other("clipboard helper failed"))
                    };
                }
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "clipboard helper timed out",
                ));
            }
            if !progressed {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    })();
    if result.is_err() {
        stop(&mut child);
    } else {
        let _ = child.wait();
    }
    result
}

fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphical_reader_order_follows_the_client_display() {
        assert!(graphical_readers(false, false).is_empty());
        assert_eq!(
            graphical_readers(true, false),
            [Reader {
                program: "wl-paste",
                args: &["--no-newline", "--type", "image/png"],
            }]
        );
        assert_eq!(
            graphical_readers(true, true)
                .iter()
                .map(|reader| reader.program)
                .collect::<Vec<_>>(),
            ["wl-paste", "xclip"]
        );
    }

    #[test]
    fn helper_output_is_binary_safe_and_bounded_by_time() {
        let mut binary = Command::new("/bin/sh");
        binary.args(["-c", "printf 'PNG\\000bytes'"]);
        assert_eq!(
            read_bounded(&mut binary, Duration::from_secs(1)).unwrap(),
            b"PNG\0bytes"
        );

        let mut stalled = Command::new("/bin/sh");
        stalled.args(["-c", "exec sleep 10"]);
        let started = Instant::now();
        assert_eq!(
            read_bounded(&mut stalled, Duration::from_millis(50))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_reader_uses_public_png_and_bounded_tiff_conversion() {
        assert!(MACOS_PASTEBOARD_SCRIPT.contains("public.png"));
        assert!(MACOS_PASTEBOARD_SCRIPT.contains("public.tiff"));
        assert!(MACOS_PASTEBOARD_SCRIPT.contains("12000000"));
        assert!(MACOS_PASTEBOARD_SCRIPT.contains("50331648"));
    }
}
