mod access;
mod gateway;
mod pairing;
mod tailcat;

use std::io::Write;
use std::time::Duration;

use anyhow::{anyhow, Result};

use access::{AccessMode, AccessSession};
use tailcat::{StartFailure, Tailcat};

const WEB_CLIENT_URL: &str = "https://web.luvus.dev/connect";

pub(crate) use access::run_cli as run_access_cli;

/// Tailcat reference provider for the standalone browser sample. The generic
/// access gateway and UHP contract remain usable without this adapter.
pub(crate) fn run_cli(args: &[String], context: crate::i18n::cli::Context) -> Result<i32> {
    let mode = access::parse_mode(args, "usage: luvus web [--control]", context)?;
    let mut access = AccessSession::start(mode, context)?;
    let mut tailcat = Tailcat::start(access.port()).map_err(|failure| {
        anyhow!(context.text(match failure {
            StartFailure::Missing => "Tailcat v0.2.0 is required on PATH for `luvus web`.",
            StartFailure::Unsupported =>
                "The Tailcat version is unsupported; `luvus web` requires v0.2.0.",
            StartFailure::Failed => "Tailcat v0.2.0 could not start; web access stayed closed.",
        }))
    })?;
    shutdown::install();

    println!("{}  EXPERIMENTAL\n", context.text("Luvus Web"));
    println!("{:<9} {WEB_CLIENT_URL}", context.text("Open:"));
    println!("{:<9} {}", context.text("Address:"), tailcat.address());
    println!("{:<9} {}", context.text("Port:"), access.port());
    println!("{:<9} {}", context.text("Pair:"), access.pairing_code());
    println!(
        "{:<9} {}",
        context.text("Access:"),
        context.text(match mode {
            AccessMode::ReadOnly => "read-only - expires in 30 minutes",
            AccessMode::Control => "CONTROL - expires in 15 minutes",
        })
    );
    if mode == AccessMode::Control {
        println!(
            "\n{}",
            context.text(
                "Control can focus workspaces, tabs, and panes, prompt agents, or interact with existing terminals."
            )
        );
    }
    println!(
        "\n{}",
        context.text("Keep this command running. Ctrl-C closes web access; panes stay alive.")
    );
    std::io::stdout().flush()?;

    let deadline = std::time::Instant::now() + mode.ttl();
    loop {
        if shutdown::requested() || std::time::Instant::now() >= deadline {
            break;
        }
        if tailcat.try_wait()?.is_some() {
            return Err(anyhow!(
                context.text("Tailcat stopped unexpectedly; web access was closed.")
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    access.stop();
    tailcat.stop();
    println!("{}", context.text("Luvus Web access closed."));
    Ok(0)
}

#[cfg(unix)]
mod shutdown {
    use std::sync::atomic::{AtomicBool, Ordering};

    static REQUESTED: AtomicBool = AtomicBool::new(false);

    pub(super) fn requested() -> bool {
        REQUESTED.load(Ordering::Relaxed)
    }

    pub(super) fn install() {
        extern "C" fn on_signal(_signal: libc::c_int) {
            REQUESTED.store(true, Ordering::Relaxed);
        }
        unsafe {
            let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
            libc::signal(libc::SIGINT, handler);
            libc::signal(libc::SIGTERM, handler);
            libc::signal(libc::SIGHUP, handler);
        }
    }
}

#[cfg(windows)]
mod shutdown {
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows_sys::Win32::Foundation::{BOOL, TRUE};
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

    static REQUESTED: AtomicBool = AtomicBool::new(false);

    unsafe extern "system" fn on_control(_kind: u32) -> BOOL {
        REQUESTED.store(true, Ordering::Relaxed);
        TRUE
    }

    pub(super) fn requested() -> bool {
        REQUESTED.load(Ordering::Relaxed)
    }

    pub(super) fn install() {
        unsafe {
            SetConsoleCtrlHandler(Some(on_control), TRUE);
        }
    }
}
