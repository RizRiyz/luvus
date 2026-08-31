//! luvus — mission control for your AI coding agents.
//! A client/server terminal workspace with live agent detection.
//! See docs/12-execution-plan.md.

mod agent;
mod api;
mod app;
mod bar;
mod changelog;
mod cli;
mod config;
mod detect;
mod diff;
mod event;
mod files;
mod git;
mod i18n;
mod ids;
mod integration;
mod ipc;
mod layout;
mod links;
mod logging;
mod mission;
mod module;
mod orch;
mod persist;
mod platform;
mod search;
mod session;
mod skill;
mod sound;
mod terminal;
mod theme;
mod uhp;
mod ui;
mod update;

use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use ratatui::crossterm::event::{
    read as read_event, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture,
    EnableBracketedPaste, EnableFocusChange, EnableMouseCapture, Event, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::DefaultTerminal;

use crate::app::App;
use crate::event::AppEvent;

const SERVER_CONTROL_TIMEOUT: Duration = Duration::from_secs(1);
/// A second, longer control-plane probe prevents a transiently busy app loop
/// from being classified as dead after one ordinary request deadline.
const SERVER_RECOVERY_TIMEOUT: Duration = Duration::from_secs(4);

fn main() -> Result<()> {
    // Run the whole process at 1ms timer resolution so the event loop's timed
    // waits aren't quantized to Windows' ~15.6ms default (the cause of laggy
    // typing in panes there). No-op on Unix; restored when `main` returns.
    let _timer = platform::high_res_timer();
    let raw_args: Vec<String> = std::env::args().collect();
    let args = session::configure_from_args(&raw_args).map_err(anyhow::Error::msg)?;
    match args.get(1).map(String::as_str) {
        // Standard CLI conveniences (don't start the server).
        Some("--version") | Some("-V") => {
            println!("luvus {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("--help") | Some("-h") => {
            let help = [args[0].clone(), "help".to_string()];
            std::process::exit(cli::run(&help)?);
        }
        Some(_) if cli::is_help_request(&args) => std::process::exit(cli::run(&args)?),
        _ => {}
    }

    // Protocol discovery is a deliberately narrow, read-only startup route.
    // Keep it ahead of every migration and setup hook so an adapter can list
    // endpoints without changing the selected Luvus home in any way.
    if is_backend_discovery_request(&args) {
        std::process::exit(cli::run(&args)?);
    }

    // One-time local cleanup of the old default-on skill installation. This
    // never downloads or installs a skill; it only removes legacy managed
    // global pointers and exact known auto-installed files.
    let _ = skill::migrate_legacy_installation();
    match args.get(1).map(String::as_str) {
        Some("server") => return server_cmd(&args),
        Some("client") => return ipc::client::run(&persist::client_socket_path()),
        // Remote attach (docs/18 RA): the bridge runs on the remote host (via
        // ssh); `--remote <host>` launches it from the local side.
        Some("remote-client-bridge") => return remote_client_bridge(),
        Some("--remote") => return remote_attach(&args),
        // `attach <id>` (docs/18 WA-2): focus + zoom the pane, then open the TUI
        // straight into that fullscreen terminal.
        Some("attach") => return attach_cmd(&args),
        Some("integration") => {
            std::process::exit(integration::run(&args, i18n::cli::Context::configured())?)
        }
        Some("--local") => return run_local(),
        Some(_) if cli::is_cli(&args) => {
            let code = cli::run(&args)?;
            std::process::exit(code);
        }
        _ => {}
    }
    // Default: attach to the session server, spawning it if needed.
    autodetect_and_attach()
}

fn is_backend_discovery_request(args: &[String]) -> bool {
    matches!(
        args,
        [_, session, list, json]
            if session == "session" && list == "list" && json == "--json"
    ) || matches!(args, [_, uhp, schema] if uhp == "uhp" && schema == "schema")
}

/// After `ratatui::init()` (which restores raw mode + alt-screen on panic), also
/// disable mouse capture and bracketed paste on panic — otherwise a crash leaves
/// the terminal in mouse-tracking mode, spewing `…;…M` sequences into the shell.
pub(crate) fn install_tui_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(
            std::io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
            PopKeyboardEnhancementFlags
        );
        prev(info);
    }));
}

/// Ask the host terminal to report **modified keys unambiguously** (the Kitty
/// keyboard protocol, via crossterm's `DISAMBIGUATE_ESCAPE_CODES`).
///
/// Legacy terminal encoding has no room for modifiers on `Enter`: the terminal
/// sends a bare `CR` for Enter *and* Shift+Enter, so luvus literally cannot tell
/// them apart and an agent's "new line, don't submit" key never works. With this
/// pushed, a capable terminal (Ghostty, Kitty, WezTerm, foot, rio, recent
/// iTerm2) reports `Shift+Enter` as its own key, which `encode_key` forwards to
/// the pane as `ESC CR`.
///
/// Only `DISAMBIGUATE_ESCAPE_CODES` is requested — deliberately *not*
/// `REPORT_EVENT_TYPES` (key-release events) or `REPORT_ALL_KEYS_AS_ESCAPE_CODES`
/// (which would stop plain text arriving as `Char`). Pushed only when the
/// terminal advertises support, so nothing is emitted into a terminal that would
/// print it as garbage, and popped on teardown (including the panic hook).
pub(crate) fn push_key_protocol() {
    use ratatui::crossterm::terminal::supports_keyboard_enhancement;
    if matches!(supports_keyboard_enhancement(), Ok(true)) {
        let _ = execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
}

/// Raise a desktop notification for terminals that show one (iTerm2, etc.).
///
/// Deliberately emits **no terminal bell** (`BEL`, 0x07): the bell beeped and —
/// with macOS Terminal's "visual bell" — flashed the whole screen on every agent
/// transition, which made the UX far worse than the alert was worth. We send
/// only `OSC 9`, terminated with `ST` (`ESC \`) rather than `BEL`, so not a
/// single `BEL` byte reaches the terminal and nothing can flash.
pub(crate) fn emit_notification(msg: &str) {
    use std::io::Write;
    let safe: String = msg.chars().filter(|c| !c.is_control()).take(120).collect();
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "\x1b]9;{safe}\x1b\\");
    let _ = out.flush();
}

/// Copy `text` to the system clipboard (pane mouse-selection → release).
///
/// Two paths, because each covers the other's gaps:
/// 1. The **native OS clipboard tool** (`pbcopy` / `wl-copy` / `xclip` / `clip`).
///    The client always runs on the user's machine — even with `--remote` — so
///    this lands in the *local* clipboard and works no matter the terminal.
/// 2. **OSC 52** — a terminal escape; covers terminals that bridge it and setups
///    where no clipboard tool is installed. Harmless if unsupported.
pub(crate) fn emit_clipboard(text: &str) {
    let _ = system_clipboard_copy(text);

    use std::io::Write;
    let b64 = base64_encode(text.as_bytes());
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "\x1b]52;c;{b64}\x1b\\");
    let _ = out.flush();
}

/// Play a synthesized notification cue. Playback stays client-side so remote
/// sessions ring where the user is sitting, not on the server host.
pub(crate) fn emit_sound(signal: sound::SoundSignal) {
    std::thread::spawn(move || {
        if let Some(path) = ensure_sound(signal) {
            play_sound_file(&path);
        }
    });
}

/// Synthesize a cue once per machine and reuse its small WAV file thereafter.
fn ensure_sound(signal: sound::SoundSignal) -> Option<std::path::PathBuf> {
    let path = sound_cache_dir()?.join(format!(
        "luvus-sound-v1-{}-{}.wav",
        signal.style.key(),
        match signal.cue {
            sound::SoundCue::Done => "done",
            sound::SoundCue::Blocked => "blocked",
        }
    ));
    let wav = sound::synth_wav(signal);
    if publish_sound_cache(&path, &wav) {
        Some(path)
    } else {
        None
    }
}

/// Return the shared, application-owned directory for synthesized cues.
///
/// The user-specific name lets attached clients reuse the cache without
/// trusting a predictable file placed directly in a system-wide temp folder.
fn sound_cache_dir() -> Option<std::path::PathBuf> {
    #[cfg(unix)]
    let name = format!("luvus-sound-cache-{}", unsafe { libc::geteuid() });
    #[cfg(not(unix))]
    let name = "luvus-sound-cache".to_string();

    let path = std::env::temp_dir().join(name);
    ensure_private_sound_cache_dir(&path).then_some(path)
}

/// Create the cache directory privately and reject hostile pre-existing paths.
fn ensure_private_sound_cache_dir(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

        let created = std::fs::DirBuilder::new().mode(0o700).create(path);
        if created.is_err_and(|error| error.kind() != std::io::ErrorKind::AlreadyExists) {
            return false;
        }
        let Ok(mut metadata) = std::fs::symlink_metadata(path) else {
            return false;
        };
        if !metadata.file_type().is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
            return false;
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            if std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).is_err() {
                return false;
            }
            let Ok(updated) = std::fs::symlink_metadata(path) else {
                return false;
            };
            metadata = updated;
        }
        metadata.file_type().is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.permissions().mode() & 0o077 == 0
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        let created = std::fs::create_dir(path);
        if created.is_err_and(|error| error.kind() != std::io::ErrorKind::AlreadyExists) {
            return false;
        }
        let Ok(metadata) = std::fs::symlink_metadata(path) else {
            return false;
        };
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_type().is_dir()
            && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
    }

    #[cfg(not(any(unix, windows)))]
    {
        let created = std::fs::create_dir(path);
        if created.is_err_and(|error| error.kind() != std::io::ErrorKind::AlreadyExists) {
            return false;
        }
        std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
    }
}

/// Publish a complete WAV atomically. Every process writes a unique staging
/// file, then renames it into place. Concurrent clients can replace one complete
/// copy with another on Unix; on Windows the loser validates and reuses the
/// winner. A partial file is never exposed at the playback path.
fn publish_sound_cache(path: &Path, wav: &[u8]) -> bool {
    if std::fs::read(path).is_ok_and(|cached| cached == wav) {
        return true;
    }

    static STAGING_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = STAGING_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let staging = path.with_file_name(format!(".{name}.{}.{}.tmp", std::process::id(), id));

    let wrote = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)?;
        file.write_all(wav)
    })();
    if wrote.is_err() {
        let _ = std::fs::remove_file(&staging);
        return false;
    }

    if std::fs::rename(&staging, path).is_ok() {
        return true;
    }

    // Windows does not replace an existing destination. Another client may
    // have won the race, so accept only the exact bytes we intended to publish.
    let published = std::fs::read(path).is_ok_and(|cached| cached == wav);
    let _ = std::fs::remove_file(&staging);
    published
}

/// Play a WAV with the platform's audio tool (blocking — called in a thread).
fn play_sound_file(path: &Path) {
    #[cfg(not(target_os = "windows"))]
    let run = |cmd: &str, args: &[&str]| -> bool {
        platform::no_window(
            Command::new(cmd)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        )
        .status()
        .is_ok()
    };
    #[cfg(not(target_os = "windows"))]
    let owned = path.to_string_lossy().into_owned();
    #[cfg(not(target_os = "windows"))]
    let p = owned.as_str();
    #[cfg(target_os = "macos")]
    {
        let _ = run("afplay", &[p]);
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Media::Audio::{PlaySoundW, SND_FILENAME, SND_NODEFAULT};

        let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // `PlaySoundW` is synchronous, but this function already runs on the
        // notification thread. Native playback avoids launching a PowerShell
        // process and therefore cannot flash a console window per sound.
        unsafe {
            let _ = PlaySoundW(
                path.as_ptr(),
                std::ptr::null_mut(),
                SND_FILENAME | SND_NODEFAULT,
            );
        }
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        let _ = run("paplay", &[p])
            || run("aplay", &["-q", p])
            || run("ffplay", &["-nodisp", "-autoexit", "-loglevel", "quiet", p]);
    }
}

/// Pipe `text` into the first available OS clipboard command.
fn system_clipboard_copy(text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let tools: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else if cfg!(target_os = "windows") {
        &[("clip", &[])]
    } else {
        &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ]
    };
    for (cmd, args) in tools {
        let Ok(mut child) = platform::no_window(
            Command::new(cmd)
                .args(*args)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        )
        .spawn() else {
            continue; // tool not installed — try the next
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "no clipboard tool",
    ))
}

/// Minimal standard base64 (no padding-dependency crate needed).
fn base64_encode(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(A[((n >> 18) & 63) as usize] as char);
        out.push(A[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            A[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            A[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// The window title we ask the host terminal to show — exactly one "luvus".
///
/// macOS Terminal.app composes its title bar itself as
/// `cwd — <OSC title> — <process ▸ child> — size`, so an OSC title of "luvus"
/// reads "… — luvus — luvus ▸ zsh — …" (and *never* setting one is worse: the
/// title component falls back to the command line, repeating the process name
/// twice). Setting an **empty** OSC title (measured against a live
/// Terminal.app) collapses the component entirely → one clean process mention.
/// Every other terminal treats the OSC title as THE title, so they keep
/// "luvus".
pub(crate) fn window_title() -> String {
    if let Some(name) = session::active_name() {
        return format!("luvus · {name}");
    }
    match std::env::var("TERM_PROGRAM") {
        Ok(p) if p == "Apple_Terminal" => String::new(),
        _ => "luvus".to_string(),
    }
}

/// Run the app monolithically against the real terminal (dev/escape hatch).
fn run_local() -> Result<()> {
    let _logging = logging::init(logging::Role::Local);
    logging::event(
        logging::EventKind::ServerStart,
        &[logging::Field::Role(logging::Role::Local)],
    );
    let mut terminal = ratatui::init();
    install_tui_panic_hook();
    let result = run(&mut terminal);
    let _ = execute!(
        std::io::stdout(),
        PopKeyboardEnhancementFlags,
        DisableFocusChange,
        DisableMouseCapture,
        DisableBracketedPaste
    );
    ratatui::restore();
    if result? {
        print_detached_status(i18n::cli::Context::configured());
    }
    Ok(())
}

fn autodetect_and_attach() -> Result<()> {
    let sock = persist::client_socket_path();
    ensure_server_ready(&sock)?;
    // Always ask the server to open the launch folder. A *fresh* server may have
    // restored a saved session (`restore_or_new`), in which case it never saw
    // this cwd — so this cannot be skipped on the fresh path. Idempotent: if the
    // folder is already a workspace, the server just focuses it.
    open_cwd_workspace();
    ipc::client::run(&sock)
}

fn ensure_server_ready(sock: &Path) -> Result<()> {
    ensure_server_ready_with_timeouts(sock, SERVER_CONTROL_TIMEOUT, SERVER_RECOVERY_TIMEOUT)
}

fn ensure_server_ready_with_timeouts(
    sock: &Path,
    control_timeout: Duration,
    recovery_timeout: Duration,
) -> Result<()> {
    match ipc::transport::connect_timeout(sock, control_timeout) {
        Ok(_) => match retry_control_probe(control_timeout, recovery_timeout, |timeout| {
            server_version_with_timeout(timeout)
        }) {
            Ok(running) => report_server_version(running),
            // Accept threads stay alive after the app loop dies. Connect is not
            // liveness, but one ordinary timeout is not proof of death either.
            // Recycle only after the longer confirmation probe also fails.
            Err(_) => {
                recycle_unresponsive_server_with_timeouts(sock, control_timeout, recovery_timeout)
            }
        },
        Err(error) if error.kind() == io::ErrorKind::TimedOut => {
            // A saturated client listener is not proof that the app loop died.
            // The API ping distinguishes a responsive server that should be
            // left alone from the mute-listener failure this recovery targets.
            if server_version_with_timeout(recovery_timeout).is_ok() {
                return Err(anyhow!(
                    "luvus client endpoint is busy but the server remains responsive; retry attach"
                ));
            }
            recycle_unresponsive_server_with_timeouts(sock, control_timeout, recovery_timeout)
        }
        Err(_) => {
            spawn_server()?;
            wait_for_socket(sock)
        }
    }
}

fn recycle_unresponsive_server_with_timeouts(
    sock: &Path,
    control_timeout: Duration,
    recovery_timeout: Duration,
) -> Result<()> {
    send_server_stop_with_evidence(
        control_timeout,
        recovery_timeout,
        RecoveryEvidence::ConfirmedUnresponsive,
    )?;
    // The old process may still own API or client pipes after stop
    // returns. Spawning now makes the replacement see those endpoints and
    // exit, then the old process dies — no server left.
    wait_for_shutdown(sock)?;
    spawn_server()?;
    wait_for_socket(sock)
}

fn retry_control_probe<T>(
    control_timeout: Duration,
    recovery_timeout: Duration,
    mut probe: impl FnMut(Duration) -> Result<T>,
) -> Result<T> {
    probe(control_timeout).or_else(|_| probe(recovery_timeout))
}

fn report_server_version(running: String) -> Result<()> {
    let binary = env!("CARGO_PKG_VERSION");
    if running != binary {
        eprintln!(
            "luvus v{binary} installed, but the running server is v{running} — \
             run `luvus server restart` to load it (your session is saved and restored)."
        );
        thread::sleep(Duration::from_millis(2000));
    }
    Ok(())
}

/// Ask the running server to open the current directory as a workspace (add +
/// focus if new). Best-effort — a failure just means no auto-open.
fn open_cwd_workspace() {
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    let Ok(mut s) =
        ipc::transport::connect_timeout(&persist::socket_path(), SERVER_CONTROL_TIMEOUT)
    else {
        return;
    };
    // `focus: false` — add the launch folder if it isn't already a workspace, but
    // don't switch to it if it is: a restored session should reopen on the
    // workspace you were last using, not snap back to the launch directory.
    let req = serde_json::json!({
        "id": "1",
        "method": "workspace.open",
        "params": { "path": cwd.display().to_string(), "focus": false },
    });
    let _ = writeln!(s, "{req}");
    let _ = ipc::api::read_response_frame_with_deadline(&mut s, SERVER_CONTROL_TIMEOUT);
}

/// Remote bridge role (docs/18 RA-1), run *on the remote host* by ssh. Ensure a
/// server is up, then pump this process's stdin/stdout to/from the local socket
/// so the `luvus --remote` client on the other end of the ssh pipe drives it.
fn remote_client_bridge() -> Result<()> {
    let sock = persist::client_socket_path();
    ensure_server_ready(&sock)?;
    ipc::client::remote_bridge(&sock)
}

/// `luvus attach <id>` (docs/18 WA-2): focus + zoom the pane (one round-trip via
/// `attach.pane`), then attach the client so it opens straight into that
/// fullscreen terminal. Composes with `--remote` for a remote fullscreen attach.
fn attach_cmd(args: &[String]) -> Result<()> {
    let sock = persist::client_socket_path();
    ensure_server_ready(&sock)?;
    if let Some(id) = args.get(2).filter(|s| s.parse::<u32>().is_ok()) {
        let _ = cli::request_attach(id); // best-effort; still attaches if it fails
    }
    ipc::client::run(&sock)
}

/// `luvus --remote <host> [ssh args]` (docs/18 RA-2): bridge a remote session's
/// socket through plain ssh and attach to it locally. No port-forwarding, no
/// `~/.ssh/config` edits — keepalive options are passed on argv only.
fn remote_attach(args: &[String]) -> Result<()> {
    let mut cmd = remote_ssh_command(args)?;
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()); // stderr inherited so ssh can prompt for auth
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("failed to launch ssh: {e}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("no ssh stdout"))?;
    let stdin = child.stdin.take().ok_or_else(|| anyhow!("no ssh stdin"))?;
    let result = ipc::client::attach(stdout, stdin);
    let _ = child.kill();
    let _ = child.wait();
    result
}

/// Build the remote bridge command separately so session propagation stays
/// testable without opening an SSH connection.
fn remote_ssh_command(args: &[String]) -> Result<Command> {
    let host = args
        .get(2)
        .ok_or_else(|| anyhow!("usage: luvus --remote <host> [ssh args]"))?;
    let mut cmd = Command::new("ssh");
    cmd.arg("-T")
        .arg("-o")
        .arg("ServerAliveInterval=15")
        .arg("-o")
        .arg("ServerAliveCountMax=3");
    // Any extra args (e.g. `-p 2222`, `-i key`) go to ssh, before the host.
    for extra in args.iter().skip(3) {
        cmd.arg(extra);
    }
    cmd.arg(host).arg("luvus");
    if let Some(name) = session::active_name() {
        cmd.arg("--session").arg(name);
    } else if session::explicit_session_requested() {
        cmd.arg("--session").arg(session::DEFAULT_SESSION_NAME);
    }
    cmd.arg("remote-client-bridge").stderr(Stdio::inherit());
    Ok(cmd)
}

fn server_running(sock: &Path) -> bool {
    ipc::transport::endpoint_exists(sock, Duration::from_millis(50))
}

fn spawn_server() -> Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("server")
        // The selector was already resolved into LUVUS_SESSION. A parent pane's
        // injected API socket must not leak into a newly spawned server.
        .env_remove("LUVUS_SOCKET_PATH")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Detach so the server survives the client exiting.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP — no console, own group.
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    cmd.spawn()?;
    Ok(())
}

fn wait_for_socket(sock: &Path) -> Result<()> {
    for _ in 0..100 {
        if server_running(sock) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(anyhow!("luvus server did not start in time"))
}

/// `luvus server <start|stop|restart|status>` — manage the background server.
/// Bare `luvus server` (no subcommand) is the internal headless role that
/// `spawn_server` launches via setsid; users go through the subcommands.
fn server_cmd(args: &[String]) -> Result<()> {
    let Some(command) = args.get(2).map(String::as_str) else {
        return ipc::server::run(); // internal role: run the server in the foreground
    };
    let context = i18n::cli::Context::configured();
    match command {
        "start" => server_start(context),
        "stop" => server_stop(context),
        "restart" => server_restart(context),
        "status" => server_status(context),
        "update-manifest" => update_manifest(context),
        other => {
            eprintln!("{}: {other}", context.text("unknown server command"));
            eprintln!(
                "{}",
                i18n::cli::help(
                    "usage: luvus server <start|stop|restart|status|update-manifest>",
                    context.language(),
                )
            );
            std::process::exit(2);
        }
    }
}

/// The published agent-detection manifest index (`luvus.dev/manifests/index.json`).
#[derive(serde::Deserialize)]
struct ManifestIndex {
    files: Vec<String>,
}

/// `luvus server update-manifest` — fetch the latest agent-detection manifests
/// and merge them in, so detection keeps up with agent-CLI UI changes without a
/// binary release. Files land in the **managed** manifest dir (never touching a
/// user's own manifests) and apply live if a server is running, else on next
/// start. The source is `https://luvus.dev/manifests` (override with
/// `$LUVUS_MANIFEST_URL`, e.g. a `file://` dir for testing).
fn update_manifest(context: i18n::cli::Context) -> Result<()> {
    let base = std::env::var("LUVUS_MANIFEST_URL")
        .unwrap_or_else(|_| "https://luvus.dev/manifests".to_string());
    let index_url = format!("{base}/index.json");
    let index_body = crate::module::discovery::http_get(&index_url)
        .map_err(|e| anyhow!("could not fetch the manifest index ({index_url}): {e}"))?;
    let index: ManifestIndex = serde_json::from_str(&index_body)
        .map_err(|e| anyhow!("bad manifest index at {index_url}: {e}"))?;

    let managed = persist::manifests_dir().join("managed");
    std::fs::create_dir_all(&managed)
        .map_err(|e| anyhow!("cannot create {}: {e}", managed.display()))?;

    let (mut written, mut skipped) = (0usize, 0usize);
    for name in &index.files {
        // Only plain `<agent>.toml` names — never a path that could escape the dir.
        let bad = !name.ends_with(".toml")
            || name.contains('/')
            || name.contains('\\')
            || name.contains("..");
        if bad {
            eprintln!(
                "{}: {name}",
                context.text("skipping suspicious manifest name")
            );
            skipped += 1;
            continue;
        }
        let body = match crate::module::discovery::http_get(&format!("{base}/{name}")) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("{} {name}: {e}", context.text("skipping"));
                skipped += 1;
                continue;
            }
        };
        // Reject a garbled download before it can land in the managed dir.
        if !crate::detect::manifest_parses(&body) {
            eprintln!(
                "{} {name}: {}",
                context.text("skipping"),
                context.text("not a valid detection manifest")
            );
            skipped += 1;
            continue;
        }
        std::fs::write(managed.join(name), body.as_bytes())
            .map_err(|e| anyhow!("cannot write {name}: {e}"))?;
        written += 1;
    }
    println!(
        "{} {written} {}{} -> {}",
        context.text("updated"),
        context.text("detection manifest(s)"),
        if skipped > 0 {
            format!(", {skipped} {}", context.text("skipped"))
        } else {
            String::new()
        },
        managed.display()
    );

    // Apply live if a server is up; otherwise the new rules load on next start.
    match crate::cli::send_request("manifest.reload", serde_json::json!({})) {
        Ok(v) => {
            let n = v
                .get("result")
                .and_then(|r| r.get("rules"))
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            println!(
                "{} ({n} {}) - {}",
                context.text("reloaded into the running server"),
                context.text("rules active"),
                context.text("no restart needed")
            );
        }
        Err(_) => println!(
            "{}",
            context.text("no server running - the update loads on next start")
        ),
    }
    Ok(())
}

/// Spawn the detached server if one isn't already up.
fn server_start(context: i18n::cli::Context) -> Result<()> {
    let sock = persist::client_socket_path();
    if server_running(&sock) {
        print_server_card(context, context.text("running"), None, &sock);
        return Ok(());
    }
    spawn_server()?;
    wait_for_socket(&sock)?;
    print_server_card(
        context,
        context.text("started"),
        Some(env!("CARGO_PKG_VERSION")),
        &sock,
    );
    Ok(())
}

fn server_stop(context: i18n::cli::Context) -> Result<()> {
    let sock = persist::client_socket_path();
    if send_server_stop()? {
        // The server acks before it actually exits, so wait for it to release the
        // socket — then `stop` returning means it's really down (and a following
        // `status` reports "not running", not a half-shutdown "running").
        wait_for_shutdown(&sock)?;
        print_server_card(context, context.text("server stopped"), None, &sock);
    } else {
        print_server_card(
            context,
            context.text("no luvus server running"),
            None,
            &sock,
        );
    }
    Ok(())
}

/// Stop (if running), wait for the socket to close, then start a fresh server —
/// the way to load a newly-installed binary without rebooting a live session.
fn server_restart(context: i18n::cli::Context) -> Result<()> {
    let sock = persist::client_socket_path();
    if send_server_stop()? {
        wait_for_shutdown(&sock)?;
    }
    spawn_server()?;
    wait_for_socket(&sock)?;
    print_server_card(
        context,
        context.text("restarted"),
        Some(env!("CARGO_PKG_VERSION")),
        &sock,
    );
    Ok(())
}

/// Poll (bounded) until the server releases its socket, so `stop`/`restart`
/// return only once the old server is truly gone.
fn wait_for_shutdown(_sock: &Path) -> Result<()> {
    let api = persist::socket_path();
    let client = persist::client_socket_path();
    for _ in 0..50 {
        if !ipc::transport::endpoint_exists(&api, Duration::from_millis(100))
            && !ipc::transport::endpoint_exists(&client, Duration::from_millis(100))
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(anyhow!(
        "luvus server did not stop in time; refusing to start a second server"
    ))
}

/// Report whether a server is up and, if so, the version it's *running* — which
/// can differ from this binary when a new install hasn't been restarted yet.
fn server_status(context: i18n::cli::Context) -> Result<()> {
    let sock = persist::client_socket_path();
    if !server_running(&sock) {
        print_server_card(context, context.text("not running"), None, &sock);
        return Ok(());
    }
    match server_version() {
        Ok(running) => {
            print_server_card(context, context.text("running"), Some(&running), &sock);
            let binary = env!("CARGO_PKG_VERSION");
            if running != binary {
                println!(
                    "  {} v{binary} — {}",
                    context.text("note: this binary is"),
                    context.text("run `luvus server restart` to load it")
                );
            }
        }
        Err(error) => {
            return Err(anyhow!(
                "luvus {}: {error}",
                context.text("server is running but did not answer")
            ));
        }
    }
    Ok(())
}

fn print_server_card(
    context: i18n::cli::Context,
    state: &str,
    version: Option<&str>,
    socket: &Path,
) {
    let session = session::display_name();
    let socket = socket.display().to_string();
    let version = version.map(|value| format!("v{value}"));
    let mut rows = vec![
        (context.text("status"), state),
        (context.text("session"), session.as_str()),
    ];
    if let Some(version) = version.as_deref() {
        rows.push((context.text("version"), version));
    }
    rows.push((context.text("socket"), socket.as_str()));
    cli::print_status_card("Luvus server", &rows);
}

fn print_detached_status(context: i18n::cli::Context) {
    let session = session::display_name();
    let runtime = format!("{} + {}", context.text("server"), context.text("panes"));
    let rows = [
        (context.text("status"), context.text("detached")),
        (context.text("session"), session.as_str()),
        (runtime.as_str(), context.text("running")),
    ];
    cli::print_status_card("Luvus session", &rows);
}

/// Send `server.stop` to a running server; returns whether one was present.
fn send_server_stop() -> Result<bool> {
    send_server_stop_with_timeouts(SERVER_CONTROL_TIMEOUT, SERVER_RECOVERY_TIMEOUT)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum RecoveryEvidence {
    RequireConfirmation,
    ConfirmedUnresponsive,
}

fn send_server_stop_with_timeouts(
    control_timeout: Duration,
    recovery_timeout: Duration,
) -> Result<bool> {
    send_server_stop_with_evidence(
        control_timeout,
        recovery_timeout,
        RecoveryEvidence::RequireConfirmation,
    )
}

fn send_server_stop_with_evidence(
    control_timeout: Duration,
    recovery_timeout: Duration,
    evidence: RecoveryEvidence,
) -> Result<bool> {
    let client_socket = persist::client_socket_path();
    let api = persist::socket_path();
    let (conn, timed_out) = match ipc::transport::connect_timeout(&api, control_timeout) {
        Ok(conn) => (Some(conn), false),
        Err(error) => match ipc::transport::connect_timeout(&client_socket, control_timeout) {
            Ok(conn) => (Some(conn), error.kind() == io::ErrorKind::TimedOut),
            Err(client_error) if client_error.kind() == io::ErrorKind::TimedOut => (None, true),
            Err(_) if error.kind() == io::ErrorKind::TimedOut => (None, true),
            Err(_) => return Ok(false),
        },
    };
    let pid = conn
        .as_ref()
        .and_then(|conn| conn.server_pid().ok())
        .or_else(persist::ServerPidFile::read);
    drop(conn);

    if timed_out {
        if evidence == RecoveryEvidence::RequireConfirmation
            && server_control_request_with_timeout("ping", recovery_timeout).is_ok()
        {
            return Err(anyhow!(
                "luvus endpoint is busy but the server remains responsive; refusing to force-stop"
            ));
        }
        return force_stop_unresponsive(pid, &client_socket);
    }

    let response = match server_control_request_with_timeout("server.stop", control_timeout) {
        Ok(response) => response,
        Err(error) => {
            // A mute app loop still accepts on dedicated listener threads.
            // One short probe: if both endpoints are already gone, stop won.
            // Otherwise reclaim the stoppable process instead of hanging attach.
            if !ipc::transport::endpoint_exists(&api, Duration::from_millis(50))
                && !ipc::transport::endpoint_exists(&client_socket, Duration::from_millis(50))
            {
                return Ok(true);
            }
            // A reachable server may simply have missed the ordinary one-second
            // acknowledgement deadline under load. Confirm the control plane is
            // still mute with a longer ping before taking the destructive path.
            if evidence == RecoveryEvidence::RequireConfirmation
                && server_control_request_with_timeout("ping", recovery_timeout).is_ok()
            {
                return Err(anyhow!(
                    "luvus server did not acknowledge stop but remains responsive: {error}"
                ));
            }
            if !ipc::transport::endpoint_exists(&api, Duration::from_millis(50))
                && !ipc::transport::endpoint_exists(&client_socket, Duration::from_millis(50))
            {
                return Ok(true);
            }
            match force_stop_unresponsive(pid, &client_socket) {
                Ok(_) => return Ok(true),
                Err(recovery_error) => {
                    return Err(anyhow!(
                        "{error}; force-stop recovery failed: {recovery_error}"
                    ));
                }
            }
        }
    };
    let acknowledged = response
        .get("result")
        .and_then(|result| result.get("type"))
        .and_then(serde_json::Value::as_str)
        == Some("ok");
    if !acknowledged {
        return Err(anyhow!("luvus server returned an invalid stop response"));
    }
    Ok(true)
}

fn force_stop_unresponsive(pid: Option<u32>, sock: &Path) -> Result<bool> {
    let Some(pid) = pid else {
        return Err(anyhow!(
            "luvus server did not answer and no process id is available to force-stop"
        ));
    };
    if !platform::is_stoppable_luvus_pid(pid) {
        return Err(anyhow!(
            "luvus server did not answer and pid {pid} is not a stoppable Luvus process"
        ));
    }
    if let Err(error) = platform::force_terminate(pid) {
        if platform::is_stoppable_luvus_pid(pid) {
            return Err(anyhow!("failed to force-stop luvus pid {pid}: {error}"));
        }
    }
    wait_for_shutdown(sock)?;
    Ok(true)
}

/// Ask the running server its version via `ping`.
fn server_version() -> Result<String> {
    server_version_with_timeout(SERVER_CONTROL_TIMEOUT)
}

fn server_version_with_timeout(timeout: Duration) -> Result<String> {
    let response = server_control_request_with_timeout("ping", timeout)?;
    response
        .get("result")
        .and_then(|result| result.get("version"))
        .and_then(serde_json::Value::as_str)
        .map(String::from)
        .ok_or_else(|| anyhow!("luvus server returned an invalid ping response"))
}

/// Perform one lifecycle request with a bounded response wait. This keeps
/// `status`, `stop`, and `restart` responsive when a socket exists but the app
/// loop cannot answer, including through Windows named pipes.
fn server_control_request_with_timeout(
    method: &str,
    timeout: Duration,
) -> Result<serde_json::Value> {
    let mut stream = ipc::transport::connect_timeout(&persist::socket_path(), timeout)
        .map_err(|error| anyhow!("cannot connect to luvus server: {error}"))?;
    writeln!(stream, r#"{{"id":"1","method":"{method}","params":{{}}}}"#)?;
    let frame = ipc::api::read_response_frame_with_deadline(&mut stream, timeout)?;
    let response: serde_json::Value = serde_json::from_str(&frame)
        .map_err(|error| anyhow!("invalid server control response: {error}"))?;
    if response.get("id").and_then(serde_json::Value::as_str) != Some("1") {
        return Err(anyhow!("server control response id does not match request"));
    }
    if let Some(error) = response.get("error") {
        return Err(anyhow!("server rejected control request: {error}"));
    }
    Ok(response)
}

fn run(terminal: &mut DefaultTerminal) -> Result<bool> {
    let (tx, rx) = mpsc::channel::<AppEvent>();

    let size = terminal.size()?;
    // Rough initial PTY size; the first draw resizes it to the exact pane rect.
    let cols = size.width.saturating_sub(34).max(20);
    let rows = size.height.saturating_sub(4).max(4);

    // `--local` still exposes the control API, so it must obey the same
    // single-server ownership rules as the headless server.
    let state_dir = persist::ensure_server_session_dir()?;
    let startup_lock = ipc::transport::acquire_server_startup_lock(&state_dir)?;
    let sock = persist::socket_path();
    let client_sock = persist::client_socket_path();
    if ipc::transport::endpoint_exists(&sock, Duration::from_millis(50))
        || ipc::transport::endpoint_exists(&client_sock, Duration::from_millis(50))
    {
        return Err(anyhow!(
            "a Luvus server is already active for {}; use `luvus` to attach to it",
            state_dir.display()
        ));
    }

    let events = ipc::api::new_bus();
    let api_listener = ipc::api::bind_server(&sock, &startup_lock)?;

    // Advertise the socket before spawning panes so they inherit LUVUS_SOCKET_PATH.
    ipc::api::set_socket_path(sock.clone());
    let mut app = match App::restore_or_new(cols, rows, tx.clone()) {
        Ok(app) => app,
        Err(err) => {
            drop(api_listener);
            let _ = remove_unbound_socket(&sock);
            return Err(err);
        }
    };
    app.events = events.clone();
    app.set_color_mode(ipc::protocol::truecolor_supported());
    let pending = if app.config.theme == "terminal" {
        let probe = terminal::theme_probe::probe();
        if let Some(colors) = probe.colors.as_ref() {
            app.apply_terminal_colors(colors);
        }
        probe.pending
    } else {
        Vec::new()
    };
    // Match the client path: query colors before enabling input protocols, so
    // any interleaved bytes are ordinary keys that can be replayed losslessly.
    let _ = execute!(
        std::io::stdout(),
        EnableBracketedPaste,
        EnableMouseCapture,
        EnableFocusChange,
        crossterm::terminal::SetTitle(window_title())
    );
    push_key_protocol();
    {
        let tx = tx.clone();
        thread::spawn(move || input_loop(tx, pending));
    }
    ipc::api::start_server(api_listener, tx.clone(), events);
    drop(startup_lock);
    app.run_module_startup_hooks(); // docs/13 §3.7 — same point as the server role

    // Background "update available" check (off if the user disabled it).
    if app.config.check_updates {
        update::spawn_check(tx.clone());
    }

    terminal.draw(|f| ui::render(f, &mut app))?;
    let mut last_draw = Instant::now();
    let mut last_save = Instant::now();
    let mut immediate_save_attempted = false;
    let mut last_spin = Instant::now();

    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(ev) => {
                app.handle_event(ev); // --local redraws every loop, so ignore the dirty bool
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        // Coalesce any queued events before drawing.
        while let Ok(ev) = rx.try_recv() {
            app.handle_event(ev);
        }
        // Parked `wait.output` deadlines lapse on the tick (docs/81).
        app.tick_output_waits(Instant::now());
        app.tick_agent_waits(Instant::now());
        app.tick_agent_workflows(Instant::now());
        app.tick_backend_revision_waits(Instant::now());
        if app.should_quit || app.detach_requested {
            break;
        }

        if !app.persist_session_now {
            immediate_save_attempted = false;
        }
        // Closing the final project bypasses the debounce once. Failed writes
        // retain both flags and retry at the normal cadence instead of hot-looping.
        let immediate_save_due = app.persist_session_now && !immediate_save_attempted;
        let debounced_save_due = app.session_dirty && last_save.elapsed() > Duration::from_secs(2);
        if immediate_save_due || debounced_save_due {
            immediate_save_attempted = app.persist_session_now;
            if persist::save(&app) {
                app.persist_session_now = false;
                app.session_dirty = false;
                immediate_save_attempted = false;
            }
            last_save = Instant::now();
        }

        // Cap redraws at ~60fps.
        let since = last_draw.elapsed();
        if since < Duration::from_millis(16) {
            thread::sleep(Duration::from_millis(16) - since);
        }
        app.detect_tick(Instant::now());
        for msg in app.pending_notify.drain(..) {
            emit_notification(&msg);
        }
        if let Some(signal) = app.pending_sound.take() {
            emit_sound(signal);
        }
        // Advance the working spinner ~10x/s (the loop redraws every frame).
        if last_spin.elapsed() >= Duration::from_millis(100)
            && (app.any_working() || app.bar.has_visible_working(&app.config.bars, app.compact))
        {
            app.spinner = app.spinner.wrapping_add(1);
            last_spin = Instant::now();
        }
        if let Some(url) = app.pending_open_url.take() {
            crate::platform::open_url(&url);
        }
        if let Some(text) = app.pending_clipboard.take() {
            emit_clipboard(&text);
        }
        app.tick_toast(Instant::now());
        app.tick_search_flash(Instant::now());
        app.tick_bar_notifications(Instant::now());
        // A forced redraw (resize / regained focus) wipes the terminal so the next
        // draw repaints every cell, healing damage ratatui's own diff can't see.
        if std::mem::take(&mut app.force_redraw) {
            terminal.clear()?;
        }
        // Don't touch the cursor here — ratatui shows + positions it once per
        // draw. A per-frame `Hide` flickered it on any activity.
        terminal.draw(|f| ui::render(f, &mut app))?;
        last_draw = Instant::now();
        // Re-arm PTY wake coalescing each frame (`--local` renders every loop
        // iteration, so the frame cadence is the re-arm cadence here).
        app.rearm_pty_notify();
    }

    let detached = app.detach_requested;
    persist::save(&app);
    Ok(detached)
}

/// Clean up a just-bound Unix socket before a local startup aborts. The caller
/// still owns the startup lock; Windows named pipes have no filesystem path.
fn remove_unbound_socket(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        let _ = path;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }
}

fn input_loop(tx: Sender<AppEvent>, pending: Vec<Event>) {
    for event in pending {
        let Some(event) = app_event(event) else {
            continue;
        };
        if tx.send(event).is_err() {
            return;
        }
    }
    while let Ok(event) = read_event() {
        let sent = match app_event(event) {
            Some(event) => tx.send(event),
            None => Ok(()),
        };
        if sent.is_err() {
            break;
        }
    }
}

fn app_event(event: Event) -> Option<AppEvent> {
    match event {
        Event::Key(k) => Some(AppEvent::Key(k)),
        Event::Mouse(m) => Some(AppEvent::Mouse(m)),
        Event::Resize(_, _) => Some(AppEvent::Resize),
        Event::Paste(s) => Some(AppEvent::Paste(s)),
        // Regained focus: treat like a resize to the current size, which forces
        // a full repaint and clears any stale cells from a move/expose.
        Event::FocusGained => crossterm::terminal::size().ok().map(|_| AppEvent::Resize),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn send_server_stop_reports_absent_when_no_sockets() {
        let _env = crate::persist::test_env("stop-absent");
        crate::persist::ensure_session_dir();
        assert!(!send_server_stop().expect("absent server is not an error"));
    }

    #[test]
    fn send_server_stop_fails_closed_when_a_listener_never_replies() {
        let _env = crate::persist::test_env("stop-mute-accept");
        crate::persist::ensure_session_dir();
        let _listener =
            crate::ipc::transport::bind(&crate::persist::socket_path()).expect("mute API listener");
        let started = Instant::now();
        let result =
            send_server_stop_with_timeouts(Duration::from_millis(40), Duration::from_millis(120));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "mute accept must not hang server stop"
        );
        assert!(
            result.is_err(),
            "fail closed without a stoppable foreign pid: {result:?}"
        );
    }

    #[test]
    fn ensure_server_ready_fails_closed_when_control_ping_never_replies() {
        let _env = crate::persist::test_env("ready-mute-accept");
        crate::persist::ensure_session_dir();
        let client = crate::persist::client_socket_path();
        let api = crate::persist::socket_path();
        let _client_listener = crate::ipc::transport::bind(&client).expect("mute client listener");
        let _api_listener = crate::ipc::transport::bind(&api).expect("mute API listener");
        let started = Instant::now();
        let result = ensure_server_ready_with_timeouts(
            &client,
            Duration::from_millis(40),
            Duration::from_millis(120),
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "mute ping must not hang attach"
        );
        assert!(
            result.is_err(),
            "fail closed rather than attach to a mute loop: {result:?}"
        );
    }

    #[test]
    fn control_probe_retries_with_a_longer_deadline_before_recovery() {
        let ordinary = Duration::from_millis(25);
        let recovery = Duration::from_millis(150);
        let mut attempts = Vec::new();
        let result = retry_control_probe(ordinary, recovery, |timeout| {
            attempts.push(timeout);
            if attempts.len() == 1 {
                Err(anyhow!("transient timeout"))
            } else {
                Ok("0.13.1".to_string())
            }
        });

        assert_eq!(result.expect("recovery probe succeeds"), "0.13.1");
        assert_eq!(attempts, vec![ordinary, recovery]);
    }

    #[test]
    fn only_exact_json_session_list_uses_discovery_route() {
        let strings = |items: &[&str]| {
            items
                .iter()
                .map(|item| item.to_string())
                .collect::<Vec<_>>()
        };
        assert!(is_backend_discovery_request(&strings(&[
            "luvus", "session", "list", "--json"
        ])));
        assert!(!is_backend_discovery_request(&strings(&[
            "luvus", "session", "list"
        ])));
        assert!(!is_backend_discovery_request(&strings(&[
            "luvus", "session", "list", "--json", "extra"
        ])));
        assert!(is_backend_discovery_request(&strings(&[
            "luvus", "uhp", "schema"
        ])));
    }

    #[test]
    fn concurrent_sound_cache_publication_never_exposes_partial_wav() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("sound-cache-test-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = Arc::new(dir.join("cue.wav"));
        let wav = Arc::new(crate::sound::synth_wav(crate::sound::SoundSignal {
            cue: crate::sound::SoundCue::Blocked,
            style: crate::sound::SoundStyle::Retro,
        }));
        let writers_done = Arc::new(AtomicBool::new(false));

        let reader_path = Arc::clone(&path);
        let reader_wav = Arc::clone(&wav);
        let reader_done = Arc::clone(&writers_done);
        let reader = std::thread::spawn(move || {
            while !reader_done.load(Ordering::Acquire) {
                if let Ok(bytes) = std::fs::read(reader_path.as_ref()) {
                    assert_eq!(bytes, *reader_wav, "published WAV is always complete");
                }
                std::thread::yield_now();
            }
        });

        let writers: Vec<_> = (0..8)
            .map(|_| {
                let path = Arc::clone(&path);
                let wav = Arc::clone(&wav);
                std::thread::spawn(move || assert!(publish_sound_cache(&path, &wav)))
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        writers_done.store(true, Ordering::Release);
        reader.join().unwrap();

        assert_eq!(std::fs::read(path.as_ref()).unwrap(), *wav);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_file(path.as_ref()).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn sound_cache_uses_a_private_application_directory() {
        let dir = sound_cache_dir().expect("private sound cache");
        assert_eq!(dir.parent(), Some(std::env::temp_dir().as_path()));
        assert_ne!(dir, std::env::temp_dir());

        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};

            let metadata = std::fs::symlink_metadata(&dir).expect("sound cache metadata");
            assert!(metadata.file_type().is_dir());
            assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
            assert_eq!(metadata.permissions().mode() & 0o077, 0);
        }
    }

    #[test]
    fn sound_cache_rejects_a_non_directory_path() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("sound-cache-file-{}-{nonce}", std::process::id()));
        std::fs::write(&path, b"not a directory").unwrap();
        assert!(!ensure_private_sound_cache_dir(&path));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn named_session_is_visible_in_the_terminal_title() {
        let _env = crate::persist::test_env("named-session-title");
        std::env::set_var(crate::session::SESSION_ENV_VAR, "docs");
        assert_eq!(window_title(), "luvus · docs");
    }

    #[test]
    fn remote_bridge_preserves_the_selected_named_session() {
        let _env = crate::persist::test_env("named-session-remote");
        let raw = [
            "luvus",
            "--session",
            "api",
            "--remote",
            "devbox",
            "-p",
            "2222",
        ]
        .map(String::from);
        let args = crate::session::configure_from_args(&raw).unwrap();
        let command = remote_ssh_command(&args).unwrap();
        let actual: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            actual,
            [
                "-T",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-p",
                "2222",
                "devbox",
                "luvus",
                "--session",
                "api",
                "remote-client-bridge",
            ]
        );
    }

    /// Manual benchmark of the server render hot path (full UI render + in-place
    /// `diff_buffer`) — the per-frame cost during typing. Run with:
    ///   cargo test --release --features dev-tools bench_render_hotpath -- --nocapture
    #[cfg(feature = "dev-tools")]
    #[test]
    fn bench_render_hotpath() {
        use crate::ipc::protocol::{diff_buffer, frame_from_buffer};
        let (tx, _rx) = mpsc::channel::<AppEvent>();
        let (w, h) = (120u16, 40u16);
        let mut app = App::new(w, h, tx).unwrap();
        let focus = app.layout().focus;
        // Fill the focused pane with a screenful of text.
        if let Some(p) = app.panes.get(&focus) {
            if let Ok(mut e) = p.engine.lock() {
                for _ in 0..h {
                    e.advance(
                        b"the quick brown fox jumps over the lazy dog 0123 abcdefghijklmnop\r\n",
                    );
                }
            }
        }
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| ui::render(f, &mut app)).unwrap();
        let mut last = frame_from_buffer(term.backend().buffer(), None, false);

        let bench = |label: &str,
                     app: &mut App,
                     term: &mut Terminal<TestBackend>,
                     last: &mut crate::ipc::protocol::FrameData,
                     feed: &[u8]| {
            let n = 2000u32;
            let t0 = std::time::Instant::now();
            let mut total_changed = 0usize;
            for _ in 0..n {
                if let Some(p) = app.panes.get(&focus) {
                    if let Ok(mut e) = p.engine.lock() {
                        e.advance(feed);
                    }
                }
                term.draw(|f| ui::render(f, app)).unwrap();
                let runs = diff_buffer(last, term.backend().buffer());
                total_changed += runs.iter().map(|r| r.symbols.len()).sum::<usize>();
            }
            let dt = t0.elapsed();
            println!(
                "{label:>10} @ {w}x{h}: {:>10?}/frame  (~{} changed cells/frame)",
                dt / n,
                total_changed as u32 / n,
            );
        };
        println!();
        bench("typing", &mut app, &mut term, &mut last, b"x");
        bench(
            "scrolling",
            &mut app,
            &mut term,
            &mut last,
            b"the quick brown fox jumps over the lazy dog 0123 abcdefghij\r\n",
        );

        // Luvus Bar feature cost: ten bounded widgets exercise both regions,
        // compact selection, and overflow without adding IO to the draw path.
        for index in 0..10 {
            let region = if index % 2 == 0 {
                crate::bar::BarRegion::TopRight
            } else {
                crate::bar::BarRegion::BottomRight
            };
            let widget = crate::bar::BarWidget::new(
                crate::bar::BarWidgetKey::new("bench", format!("job-{index}")),
                region,
                vec![crate::bar::BarSegment::text(
                    format!("job {index} ready"),
                    crate::bar::BarTone::Success,
                )],
                vec![crate::bar::BarSegment::text(
                    format!("j{index}"),
                    crate::bar::BarTone::Success,
                )],
                index as u8,
            )
            .unwrap();
            app.bar.push_widget(widget).unwrap();
        }
        bench("10 widgets", &mut app, &mut term, &mut last, b"x");

        // Exercise the update path separately so render cost and update cost
        // remain distinguishable in manual performance runs.
        let updates = 2_000u32;
        let started = std::time::Instant::now();
        for index in 0..updates {
            let widget = crate::bar::BarWidget::new(
                crate::bar::BarWidgetKey::new("bench", "live"),
                crate::bar::BarRegion::BottomRight,
                vec![crate::bar::BarSegment::text(
                    format!("build {}", index % 100),
                    crate::bar::BarTone::Accent,
                )],
                Vec::new(),
                50,
            )
            .unwrap();
            app.bar.push_widget(widget).unwrap();
            term.draw(|frame| ui::render(frame, &mut app)).unwrap();
        }
        println!(
            "bar update @ {w}x{h}: {:>10?}/update+frame",
            started.elapsed() / updates
        );

        // Breakdown of one frame (where the ~126µs goes).
        let n = 5000u32;
        // (a) the pane grid-walk alone (alacritty display_iter → RenderCell).
        let t = std::time::Instant::now();
        for _ in 0..n {
            if let Some(p) = app.panes.get(&focus) {
                if let Ok(e) = p.engine.lock() {
                    e.for_each_cell(&mut |_, _, _, _| {});
                }
            }
        }
        let grid_walk = t.elapsed() / n;
        // (b) ratatui Terminal::draw with an EMPTY render (its reset+diff+flush overhead).
        let t = std::time::Instant::now();
        for _ in 0..n {
            term.draw(|_f| {}).unwrap();
        }
        let ratatui_overhead = t.elapsed() / n;
        // (c) the full draw (overhead + the real ui::render).
        let t = std::time::Instant::now();
        for _ in 0..n {
            term.draw(|f| ui::render(f, &mut app)).unwrap();
        }
        let full_draw = t.elapsed() / n;
        // (d) diff_buffer alone.
        let t = std::time::Instant::now();
        for _ in 0..n {
            let _ = diff_buffer(&mut last, term.backend().buffer());
        }
        let diff = t.elapsed() / n;
        // (e) the actual server frame now: render straight into an owned buffer +
        // diff, with NO ratatui Terminal in the loop.
        let area = ratatui::layout::Rect::new(0, 0, w, h);
        let mut owned = ratatui::buffer::Buffer::empty(area);
        let t = std::time::Instant::now();
        for _ in 0..n {
            owned.reset();
            {
                let mut tg = crate::ui::RenderTarget::new(&mut owned, area);
                ui::render_into(&mut tg, &mut app);
            }
            let _ = diff_buffer(&mut last, &owned);
        }
        let server_frame = t.elapsed() / n;
        println!("  breakdown:");
        println!("    pane grid-walk:    {grid_walk:>10?}");
        println!(
            "    ratatui overhead:  {ratatui_overhead:>10?}  (reset+diff+flush — now dropped)"
        );
        println!(
            "    OLD full frame:    {:>10?}  (terminal.draw + diff_buffer)",
            full_draw + diff
        );
        println!(
            "    NEW server frame:  {server_frame:>10?}  (render_into owned buf + diff_buffer)"
        );
        // (f) the CLIENT's per-frame cost: re-blit the whole frame via terminal.draw.
        let frame = frame_from_buffer(&owned, None, false);
        let mut cterm = Terminal::new(TestBackend::new(w, h)).unwrap();
        let t = std::time::Instant::now();
        for _ in 0..n {
            cterm
                .draw(|f| {
                    let b = f.buffer_mut();
                    for (i, cell) in frame.cells.iter().enumerate() {
                        let (x, y) = ((i as u16) % w, (i as u16) / w);
                        let tgt = &mut b[(x, y)];
                        tgt.set_symbol(if cell.symbol.is_empty() {
                            " "
                        } else {
                            &cell.symbol
                        });
                        tgt.set_fg(crate::ipc::protocol::unpack(cell.fg));
                        tgt.set_bg(crate::ipc::protocol::unpack(cell.bg));
                        tgt.modifier = crate::ipc::protocol::unpack_mods(cell.mods);
                    }
                })
                .unwrap();
        }
        let client_blit = t.elapsed() / n;
        println!("    CLIENT old re-blit:{client_blit:>10?}  (terminal.draw full frame — REMOVED; client now writes only changed cells)");
        println!();
    }

    /// Focused docs/100 check: compare forced desktop/mobile frames at the same
    /// viewport and measure the full-screen navigator separately. It intentionally
    /// performs no IO and creates no background task.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn bench_mobile_render_hotpath() {
        use ratatui::{backend::TestBackend, Terminal};

        let render = |label: &str, app: &mut App| {
            let mut terminal = Terminal::new(TestBackend::new(64, 35)).unwrap();
            terminal.draw(|frame| ui::render(frame, app)).unwrap();
            let frames = 5_000u32;
            let started = Instant::now();
            for _ in 0..frames {
                terminal.draw(|frame| ui::render(frame, app)).unwrap();
            }
            let elapsed = started.elapsed();
            println!("{label:>18}: {:>10?}/frame", elapsed / frames);
            elapsed / frames
        };

        let (tx, _rx) = mpsc::channel::<AppEvent>();
        let mut desktop = App::new(64, 35, tx.clone()).unwrap();
        desktop.config.layout.mobile_width = 0;
        let desktop_frame = render("desktop 64x35", &mut desktop);

        let mut mobile = App::new(64, 35, tx).unwrap();
        let mobile_frame = render("mobile closed", &mut mobile);
        mobile.open_switcher();
        let navigator_frame = render("mobile navigator", &mut mobile);

        println!(
            "mobile/desktop: {:.3}x, navigator/desktop: {:.3}x",
            mobile_frame.as_secs_f64() / desktop_frame.as_secs_f64(),
            navigator_frame.as_secs_f64() / desktop_frame.as_secs_f64(),
        );
    }

    #[test]
    fn base64_matches_known_vectors() {
        // RFC 4648 test vectors — the OSC 52 clipboard payload must encode right.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode("héllo".as_bytes()), "aMOpbGxv");
    }

    /// Render one frame of the full UI to an off-screen buffer and assert the
    /// chrome is present. Exercises App::new (real PTY spawn), the VtEngine, and
    /// every draw path — catches panics and layout regressions without a tty.
    #[test]
    fn renders_chrome() {
        let (tx, _rx) = mpsc::channel::<AppEvent>();
        let mut app = App::new(80, 24, tx).expect("spawn pane");
        // Give the shell a moment to emit its prompt into the grid.
        thread::sleep(Duration::from_millis(150));

        let backend = TestBackend::new(110, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();

        let buf = terminal.backend().buffer();
        let mut text = String::new();
        for cell in buf.content() {
            text.push_str(cell.symbol());
        }

        assert!(text.contains("luvus"), "brand missing");
        assert!(text.contains("WORKSPACES"), "workspaces header missing");
        assert!(text.contains("AGENTS"), "agents header missing");
        assert!(text.contains("tab"), "tab status missing");
        assert!(text.contains("NORMAL"), "status mode missing");
    }

    /// Naming a pane (via `pane name` / `agent name`) shows the name on the pane's
    /// title strip in place of its cwd path, so a named pane is visibly renamed.
    #[test]
    fn naming_a_pane_renames_its_title() {
        let (tx, _rx) = mpsc::channel::<AppEvent>();
        let mut app = App::new(80, 24, tx).expect("spawn pane");
        thread::sleep(Duration::from_millis(120));
        // Pane titles only render when a tab has more than one (bordered) pane.
        app.dispatch("pane.split", &serde_json::Value::Object(Default::default()))
            .unwrap();
        let pane = app.layout().focus;
        app.agent_names.insert("apisvc".into(), pane);

        let mut terminal = Terminal::new(TestBackend::new(110, 32)).unwrap();
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            text.contains("apisvc"),
            "a named pane's title should show its name"
        );
    }

    /// With the `pane_title_path` setting on, a named pane's title appends its cwd
    /// path after the name; off (default) it shows just the name.
    #[test]
    fn pane_title_path_setting_appends_the_path() {
        let _env = crate::persist::test_env("pane-title-path");
        let (tx, _rx) = mpsc::channel::<AppEvent>();
        let mut app = App::new(80, 24, tx).expect("spawn pane");
        thread::sleep(Duration::from_millis(120));
        app.dispatch("pane.split", &serde_json::Value::Object(Default::default()))
            .unwrap();
        let pane = app.layout().focus;
        app.agent_names.insert("svcx".into(), pane);

        let render = |app: &mut App| -> String {
            let mut term = Terminal::new(TestBackend::new(110, 32)).unwrap();
            term.draw(|f| ui::render(f, app)).unwrap();
            term.backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect()
        };

        // Keep the rendering assertion independent of config written by other
        // tests. The default itself belongs to Config's unit contract.
        app.config.layout.pane_title_path = false;
        let off = render(&mut app);
        assert!(off.contains("svcx"), "named pane shows its name");
        assert!(
            !off.contains("svcx  "),
            "default title is the name alone, without the path"
        );

        // Setting on: the path follows the name.
        app.config.layout.pane_title_path = true;
        let on = render(&mut app);
        assert!(
            on.contains("svcx  "),
            "with pane_title_path on, the title appends the path after the name"
        );
    }

    /// Clicking the bottom-right version number opens the changelog modal, which shows
    /// the embedded release notes; esc closes it.
    #[test]
    fn clicking_version_opens_the_changelog() {
        use ratatui::crossterm::event::{
            KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        };
        let _env = crate::persist::test_env("changelog-click");
        let (tx, _rx) = mpsc::channel::<AppEvent>();
        let mut app = App::new(80, 24, tx).expect("spawn pane");
        thread::sleep(Duration::from_millis(100));

        let (w, h) = (110u16, 34u16);
        let render = |app: &mut App| -> String {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| ui::render(f, app)).unwrap();
            term.backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect()
        };

        // The version number is a click target in the bottom status line.
        render(&mut app);
        let vr = app.version_rect.expect("version number is clickable");
        assert_eq!(vr.y, h - 1, "version belongs to the bottom status line");
        assert_eq!(vr.right() + 1, w, "version keeps one right padding cell");
        app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: vr.x + 1,
            row: vr.y,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(
            app.changelog_open,
            "clicking the version opened the changelog"
        );

        let text = render(&mut app);
        assert!(text.contains("Changelog"), "modal title shows");
        // The newest release's version header is always the first note rendered,
        // whatever sections it has (a patch release may carry only "Fixed").
        let newest = crate::changelog::CHANGELOG[0].0;
        assert!(
            text.contains(newest),
            "release notes render (newest header {newest})"
        );

        // esc closes it.
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )));
        assert!(!app.changelog_open, "esc closed the changelog");
    }

    /// The bottom-left status button shows/hides the sidebar; hiding it also
    /// clears the sidebar's stale click geometry so the old Menu spot can't fire.
    #[test]
    fn sidebar_toggle_button_shows_and_hides() {
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        // Isolate config so a concurrent config-writing test can't change the
        // sidebar layout under us via the shared `LUVUS_HOME` env var.
        let _env = crate::persist::test_env("toggle");
        let (tx, _rx) = mpsc::channel::<AppEvent>();
        let mut app = App::new(80, 24, tx).expect("spawn pane");
        thread::sleep(Duration::from_millis(100));

        let (w, h) = (110u16, 32u16);
        let render = |app: &mut App| -> String {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| ui::render(f, app)).unwrap();
            let buf = term.backend().buffer().clone();
            buf.content().iter().map(|c| c.symbol()).collect()
        };
        let click = |app: &mut App, c: u16, r: u16| {
            app.handle_event(AppEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: c,
                row: r,
                modifiers: KeyModifiers::NONE,
            }));
        };

        // Starts visible: header shows, and the `«` chevron (top-left of the
        // sidebar, on the tab-bar row) is the collapse toggle.
        let text = render(&mut app);
        assert!(text.contains("WORKSPACES"), "sidebar should start visible");
        assert!(text.contains('«'), "brand collapse chevron shows");
        let btn = app.sidebar_toggle_rect.expect("toggle placed");
        assert_eq!(
            btn.y, 0,
            "toggle sits on the top row, aligned with the tab bar"
        );
        assert!(btn.x < 4, "toggle near the top-left");
        assert!(app.settings_icon_rect.is_some(), "menu present while shown");

        // Click the chevron → sidebar hides and its stale click geometry clears.
        click(&mut app, btn.x, btn.y);
        assert!(!app.sidebars.left.visible, "click hides the sidebar");
        let text = render(&mut app);
        assert!(!text.contains("WORKSPACES"), "sidebar hidden after toggle");
        assert!(
            text.contains('»'),
            "reopen expand chevron shows when hidden"
        );
        assert!(app.settings_icon_rect.is_none(), "stale menu rect cleared");
        assert!(
            app.agents_filter_rects.is_empty(),
            "stale filter rects cleared"
        );

        // A reopen `»` now sits at the tab-bar's top-left corner.
        let btn = app
            .sidebar_toggle_rect
            .expect("reopen toggle placed while hidden");
        assert_eq!(
            (btn.x, btn.y),
            (0, 0),
            "reopen toggle at the top-left corner"
        );
        click(&mut app, btn.x, btn.y);
        assert!(app.sidebars.left.visible, "click shows the sidebar again");
        assert!(render(&mut app).contains("WORKSPACES"), "sidebar restored");
    }

    /// The ⏎-commit / esc-cancel footer of the text-input modals is clickable,
    /// driving the same commit/cancel path as the keyboard.
    #[test]
    fn modal_footer_buttons_are_clickable() {
        use ratatui::crossterm::event::{
            KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        };
        use ratatui::layout::Rect;
        let (tx, _rx) = mpsc::channel::<AppEvent>();
        let mut app = App::new(80, 24, tx).expect("spawn pane");
        thread::sleep(Duration::from_millis(100));

        let render = |app: &mut App| {
            let mut t = Terminal::new(TestBackend::new(100, 32)).unwrap();
            t.draw(|f| ui::render(f, app)).unwrap();
        };
        let click = |app: &mut App, r: Rect| {
            app.handle_event(AppEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: r.x + 1,
                row: r.y,
                modifiers: KeyModifiers::NONE,
            }));
        };
        let typ = |app: &mut App, s: &str| {
            for c in s.chars() {
                app.handle_event(AppEvent::Key(KeyEvent::new(
                    KeyCode::Char(c),
                    KeyModifiers::NONE,
                )));
            }
        };
        let clear = |app: &mut App, n: usize| {
            for _ in 0..n {
                app.handle_event(AppEvent::Key(KeyEvent::new(
                    KeyCode::Backspace,
                    KeyModifiers::NONE,
                )));
            }
        };

        // Rename: type, then click the ⏎ commit button → label changes.
        app.open_ws_rename(0);
        render(&mut app);
        let n = app.workspaces[0].name.chars().count();
        clear(&mut app, n);
        typ(&mut app, "clicked");
        render(&mut app);
        let commit = app.modal_commit_rect.expect("commit button placed");
        click(&mut app, commit);
        assert!(app.ws_rename.is_none(), "clicking ⏎ commits + closes");
        assert_eq!(
            app.workspaces[0].name, "clicked",
            "commit applied via click"
        );

        // Rename again, then click esc cancel → the edit is discarded.
        app.open_ws_rename(0);
        render(&mut app);
        typ(&mut app, "XXX");
        render(&mut app);
        let cancel = app.modal_cancel_rect.expect("cancel button placed");
        click(&mut app, cancel);
        assert!(app.ws_rename.is_none(), "clicking esc cancels + closes");
        assert_eq!(
            app.workspaces[0].name, "clicked",
            "cancel discards the edit"
        );

        // The worktree prompt's cancel button also closes it (no worktree made).
        app.worktree_prompt = Some("feature".into());
        render(&mut app);
        let cancel = app.modal_cancel_rect.expect("worktree cancel placed");
        click(&mut app, cancel);
        assert!(
            app.worktree_prompt.is_none(),
            "worktree prompt cancels via click"
        );
    }

    /// Right-clicking a WORKSPACES row opens a context menu; picking Rename edits
    /// the label (not the folder), and picking Close removes the workspace.
    #[test]
    fn workspace_context_menu_rename_and_close() {
        use crate::app::WsMenuItem;
        use ratatui::crossterm::event::{
            KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        };
        let (tx, _rx) = mpsc::channel::<AppEvent>();
        let mut app = App::new(80, 24, tx).expect("spawn pane");
        thread::sleep(Duration::from_millis(100));
        // A second workspace so closing one doesn't quit the app.
        app.create_workspace_at(std::env::temp_dir());

        let render = |app: &mut App| {
            let mut term = Terminal::new(TestBackend::new(110, 32)).unwrap();
            term.draw(|f| ui::render(f, app)).unwrap();
        };
        let mouse = |app: &mut App, btn, c: u16, r: u16| {
            app.handle_event(AppEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Down(btn),
                column: c,
                row: r,
                modifiers: KeyModifiers::NONE,
            }));
        };
        let key = |app: &mut App, code| {
            app.handle_event(AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)));
        };
        let ws_row = |app: &App| {
            app.ws_rects
                .iter()
                .find(|(i, _)| *i == 0)
                .map(|(_, r)| *r)
                .expect("workspace row rect")
        };
        let item_rect = |app: &App, want: WsMenuItem| {
            app.ws_menu
                .as_ref()
                .expect("menu open")
                .items
                .iter()
                .find(|(it, _)| *it == want)
                .map(|(_, r)| *r)
                .expect("menu item")
        };

        // Right-click the first workspace → its context menu opens.
        render(&mut app);
        let row = ws_row(&app);
        mouse(&mut app, MouseButton::Right, row.x + 1, row.y);
        assert!(app.ws_menu.is_some(), "right-click opens the menu");
        render(&mut app); // populates item rects

        // Pick Rename → the modal opens pre-filled with the current label.
        let rn = item_rect(&app, WsMenuItem::Rename);
        mouse(&mut app, MouseButton::Left, rn.x + 1, rn.y);
        assert!(app.ws_menu.is_none(), "menu closes after a pick");
        let name0 = app.workspaces[0].name.clone();
        let cwd0 = app.workspaces[0].cwd.clone();
        assert_eq!(
            app.ws_rename.as_ref().expect("rename modal").buffer,
            name0,
            "prefilled with the name"
        );
        for _ in 0..name0.chars().count() {
            key(&mut app, KeyCode::Backspace);
        }
        for ch in "renamed".chars() {
            key(&mut app, KeyCode::Char(ch));
        }
        key(&mut app, KeyCode::Enter);
        assert!(app.ws_rename.is_none(), "Enter commits + closes");
        assert_eq!(app.workspaces[0].name, "renamed", "label updated");
        assert_eq!(app.workspaces[0].cwd, cwd0, "folder path untouched");

        // Right-click again → Close removes the workspace (without quitting).
        let n = app.workspaces.len();
        render(&mut app);
        let row = ws_row(&app);
        mouse(&mut app, MouseButton::Right, row.x + 1, row.y);
        render(&mut app);
        let cl = item_rect(&app, WsMenuItem::Close);
        mouse(&mut app, MouseButton::Left, cl.x + 1, cl.y);
        assert_eq!(app.workspaces.len(), n - 1, "Close removes the workspace");
        assert!(!app.should_quit, "a workspace remains");
    }

    /// An absurdly small terminal renders the "enlarge" notice instead of
    /// degraded chrome — and no size, however tiny, panics a draw path.
    #[test]
    fn tiny_terminal_shows_guard_not_garbage() {
        let (tx, _rx) = mpsc::channel::<AppEvent>();
        let mut app = App::new(80, 24, tx).expect("spawn pane");

        for (w, h) in [(1, 1), (5, 2), (23, 5), (20, 4)] {
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|f| ui::render(f, &mut app)).unwrap(); // must not panic
        }

        // At a small-but-writable size the guard message is visible.
        let backend = TestBackend::new(20, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            text.contains("enlarge"),
            "tiny-terminal guard message missing: {text:?}"
        );
    }

    /// Remote from a phone SSH app (docs/18): the client renders luvus over the
    /// ssh PTY at whatever the phone terminal reports. Validate the viewport range
    /// a phone presents — portrait/narrow and landscape — renders without panic or
    /// garbage, dropping the sidebar when the content would fall below its minimum.
    #[test]
    fn renders_across_phone_viewports() {
        use ratatui::{backend::TestBackend, Terminal};
        let (tx, _rx) = mpsc::channel::<AppEvent>();
        let mut app = App::new(80, 24, tx).expect("spawn pane");

        let full_row = |term: &Terminal<TestBackend>, r: u16| -> String {
            let buf = term.backend().buffer();
            (0..buf.area.width)
                .map(|c| buf.cell((c, r)).map(|x| x.symbol()).unwrap_or(" "))
                .collect()
        };

        // Common phone SSH sizes: landscape ~90x24, portrait ~40x60, cramped ~34x50.
        for (w, h) in [(90u16, 24u16), (40, 60), (34, 50), (50, 40)] {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| ui::render(f, &mut app)).unwrap(); // must never panic
                                                             // Not the too-small guard at these sizes: real chrome renders.
            let all: String = (0..h).map(|r| full_row(&term, r)).collect();
            assert!(
                !all.contains("enlarge terminal"),
                "{w}x{h} is usable, not the guard"
            );
        }

        // A genuinely tiny phone-keyboard-open viewport shows the guard, not garbage.
        let mut term = Terminal::new(TestBackend::new(20, 4)).unwrap();
        term.draw(|f| ui::render(f, &mut app)).unwrap();
        let all: String = (0..4).map(|r| full_row(&term, r)).collect();
        assert!(
            all.contains("enlarge terminal"),
            "a tiny viewport gets the friendly guard"
        );
    }

    /// The orchestration board tab (docs/22, ORCH-7) renders its header, a task
    /// row, and the leases section into the off-screen buffer without panicking.
    #[test]
    fn renders_orch_board() {
        // Isolate $LUVUS_HOME: orch mutations call `orch.save()`, so without this
        // parallel orch tests race on a shared `orch.json`.
        let _env = crate::persist::test_env("renders-orch-board");
        let (tx, _rx) = mpsc::channel::<AppEvent>();
        let mut app = App::new(80, 24, tx).expect("spawn pane");
        app.orch
            .add_task(
                "Wire the auth module".into(),
                vec!["src/auth/**".into()],
                vec![],
                None,
            )
            .unwrap();
        app.orch.claim("t1", 1).unwrap();
        app.orch
            .acquire_lease(1, "t1".into(), vec!["src/auth/**".into()])
            .unwrap();
        app.open_orch_board();
        assert!(app.active_is_orch(), "board tab is active");

        let backend = TestBackend::new(110, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();

        let buf = terminal.backend().buffer();
        let mut text = String::new();
        for cell in buf.content() {
            text.push_str(cell.symbol());
        }
        assert!(text.contains("ORCHESTRATION"), "board header missing");
        assert!(text.contains("STATUS"), "task table header missing");
        assert!(text.contains("▌"), "selected task marker missing");
        assert!(text.contains("Wire the auth module"), "task title missing");
        assert!(text.contains("claimed"), "task status missing");
        assert!(text.contains("LEASES"), "leases section missing");
        assert!(text.contains("◇ orch"), "board tab label missing");
    }

    /// The board's UX layer renders: a Running worker row without duplicating
    /// live agent state, the start-worker picker, and the task detail overlay.
    #[test]
    fn renders_board_worker_binding_picker_and_detail() {
        let _env = crate::persist::test_env("renders-board-live");
        let (tx, _rx) = mpsc::channel::<AppEvent>();
        let mut app = App::new(80, 24, tx).expect("spawn pane");
        let pane = *app.panes.keys().next().unwrap();
        app.orch
            .add_task(
                "Auth worker".into(),
                vec!["src/auth/**".into()],
                vec![],
                None,
            )
            .unwrap();
        app.orch.claim("t1", pane.0).unwrap();
        app.orch
            .set_status("t1", crate::orch::TaskStatus::Running)
            .unwrap();
        app.orch
            .bind_worktree("t1", Some("/tmp/wt".into()), Some("luvus/t1".into()));
        // Live agent state belongs to the agent surfaces, not the task's Pane
        // column. Seed it here to prove the board does not duplicate it.
        if let Some(st) = app.status.get_mut(&pane) {
            st.agent = "claude".into();
            st.state = crate::ui::theme::State::Working;
        }
        app.open_orch_board();

        let render_lines = |app: &mut App| {
            let mut terminal = Terminal::new(TestBackend::new(110, 32)).unwrap();
            terminal.draw(|f| ui::render(f, app)).unwrap();
            let buf = terminal.backend().buffer().clone();
            buf.content()
                .chunks(110)
                .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
                .collect::<Vec<_>>()
        };

        let lines = render_lines(&mut app);
        let worker_row = lines
            .iter()
            .find(|line| line.contains("luvus/t1"))
            .expect("worker branch shown");
        assert!(
            worker_row.contains("running"),
            "started worker shows running"
        );
        assert!(
            !worker_row.contains("working"),
            "live agent state stays out of Pane"
        );
        assert!(
            !worker_row.contains("claude"),
            "agent name stays out of Pane"
        );

        // The start-worker picker draws over the board.
        app.orch_start = Some(crate::app::OrchStart {
            task: "t1".into(),
            cursor: 0,
            step: crate::app::OrchStartStep::Agent,
            mode: crate::orch::TaskWorkerMode::Worktree,
            shared_workers: 0,
        });
        let text = render_lines(&mut app).join("\n");
        assert!(text.contains("claude"), "picker lists agents");
        app.orch_start = None;

        // The detail overlay shows the task's binding.
        app.orch_detail = Some("t1".into());
        let text = render_lines(&mut app).join("\n");
        assert!(text.contains("/tmp/wt"), "detail shows the worktree");
    }

    /// Regression: a pane whose grid holds a control char must not panic
    /// ratatui's `cell_width`. `git status` aligns with TABs, which alacritty
    /// stores as a literal `\t` cell — `set_symbol("\t")` tripped the assert.
    #[test]
    fn renders_pane_with_tab() {
        let (tx, _rx) = mpsc::channel::<AppEvent>();
        let mut app = App::new(80, 24, tx).expect("spawn pane");
        let id = app.layout().focus;
        // Inject git-status-like output containing a TAB into the pane grid.
        app.panes
            .get(&id)
            .unwrap()
            .engine
            .lock()
            .unwrap()
            .advance(b"\tmodified:\tsrc/main.rs\r\n");
        let backend = TestBackend::new(110, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        // The bug was a panic here ("control character passed to cell_width").
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
    }

    /// A double-width emoji must mark the cell to its right as a wide-char
    /// continuation (empty symbol), not leave a blank space — otherwise the
    /// client blits that space into the glyph's second column, corrupting the
    /// emoji and shifting the rest of the row (the reported bug). Tests the real
    /// server render path (`render_into` a Buffer, which `frame_from_buffer`
    /// serializes), not a TestBackend flush — ratatui's flush hides the
    /// continuation cell, but luvus's wire protocol reads it.
    #[test]
    fn wide_emoji_marks_its_continuation_cell() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use std::sync::{Arc, Mutex};

        let _env = crate::persist::test_env("wide-emoji-frame");
        let (tx, _rx) = mpsc::channel::<AppEvent>();
        let mut app = App::new(80, 24, tx).expect("spawn pane");
        let id = app.layout().focus;
        // Detach this assertion from the live shell reader. It may write its
        // prompt between injection and rendering, which made the glyph vanish
        // nondeterministically even though the renderer was correct.
        let (response_tx, _response_rx) = mpsc::channel();
        app.panes.get_mut(&id).unwrap().engine = Arc::new(Mutex::new(
            crate::terminal::vt::alacritty::AlacrittyEngine::new(
                80,
                24,
                response_tx,
                crate::config::SCROLLBACK_BYTES_DEFAULT,
            ),
        ));
        app.panes
            .get(&id)
            .unwrap()
            .engine
            .lock()
            .unwrap()
            .advance("\x1b[H\x1b[2J\u{1F534}AB".as_bytes());
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        {
            let mut target = ui::RenderTarget::new(&mut buf, area);
            ui::render_into(&mut target, &mut app);
        }
        let mut at = None;
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if buf[(x, y)].symbol() == "\u{1F534}" {
                    at = Some((x, y));
                }
            }
        }
        let (x, y) = at.expect("the emoji rendered somewhere");
        assert_eq!(
            buf[(x + 1, y)].symbol(),
            "",
            "the wide-char continuation cell must be empty, not a space"
        );
        assert_eq!(
            buf[(x + 2, y)].symbol(),
            "A",
            "the text after the emoji is not shifted"
        );
    }

    /// The Settings → Keys tab shows the how-to intro and the rebindable command
    /// list at the top, and the cursor steps through the commands *and* the
    /// read-only reference blocks, so holding Down eventually reveals every one
    /// (the last is the Mouse block) on a short modal.
    /// Every copy-mode reference row has to render its description in full. The
    /// i18n arity check counts rows but cannot see the panel, and the panel's
    /// description column is exactly what a longer translation runs out of: the
    /// Indonesian count row was clipped mid-word at 63 characters while English
    /// fit at 48. Wide cells leave an empty continuation cell, so the comparison
    /// ignores whitespace rather than pretending CJK reads back cell-for-cell.
    #[test]
    fn copy_mode_reference_rows_are_not_clipped_in_any_language() {
        use ratatui::buffer::Buffer;
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::layout::Rect;
        let flatten = |s: &str| -> String { s.chars().filter(|c| !c.is_whitespace()).collect() };
        for code in crate::i18n::LANGS {
            let _env = crate::persist::test_env(&format!("keys-clip-{code}"));
            let (tx, _rx) = mpsc::channel::<AppEvent>();
            let mut app = App::new(80, 24, tx).expect("spawn pane");
            app.catalog = crate::i18n::by_code(code);
            app.open_settings();
            app.handle_settings_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
            let render = |app: &mut App| -> String {
                let area = Rect::new(0, 0, 80, 24);
                let mut buf = Buffer::empty(area);
                {
                    let mut target = ui::RenderTarget::new(&mut buf, area);
                    ui::render_into(&mut target, app);
                }
                let mut out = String::new();
                for y in 0..area.height {
                    for x in 0..area.width {
                        out.push_str(buf[(x, y)].symbol());
                    }
                    out.push('\n');
                }
                out
            };

            let rows = app.settings_rows(crate::app::SettingsTab::Keys);
            for _ in 0..rows {
                app.handle_settings_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            }
            let heading = flatten(app.catalog.settings.key_reference_headings[2]);
            let mut view = render(&mut app);
            for _ in 0..rows {
                if flatten(&view).contains(&heading) {
                    break;
                }
                app.handle_settings_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
                view = render(&mut app);
            }
            assert!(
                flatten(&view).contains(&heading),
                "{code} never scrolled the copy-mode block into view"
            );

            let keys = crate::i18n::settings::KEY_REFERENCE_KEYS[2];
            let descs = app.catalog.settings.key_reference_descriptions[2];
            let seen = flatten(&view);
            for (key, desc) in keys.iter().zip(descs.iter()) {
                assert!(
                    seen.contains(&flatten(desc)),
                    "{code} clips the {key} row at {} characters:\n{desc}\n{view}",
                    desc.chars().count()
                );
            }
        }
    }

    #[test]
    fn keys_tab_shows_help_and_scrolls_to_the_reference() {
        use ratatui::buffer::Buffer;
        use ratatui::crossterm::event::{
            KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind,
        };
        use ratatui::layout::Rect;
        let _env = crate::persist::test_env("keys-tab");

        let (tx, _rx) = mpsc::channel::<AppEvent>();
        // A short viewport so the tab must scroll to reveal the reference block.
        let mut app = App::new(80, 22, tx).expect("spawn pane");

        let screen = |app: &mut App| -> String {
            let area = Rect::new(0, 0, 80, 22);
            let mut buf = Buffer::empty(area);
            {
                let mut target = ui::RenderTarget::new(&mut buf, area);
                ui::render_into(&mut target, app);
            }
            let mut s = String::new();
            for y in 0..area.height {
                for x in 0..area.width {
                    s.push_str(buf[(x, y)].symbol());
                }
                s.push('\n');
            }
            s
        };

        app.open_settings();
        // Switch to the Keys tab (General·Theme·Layout·Keys → the '4' digit).
        app.handle_settings_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));

        let top = screen(&mut app);
        assert!(
            top.contains("Ctrl+Space") && top.contains("cheat-sheet"),
            "the how-to intro is visible at the top:\n{top}"
        );
        assert!(
            !top.contains("Always active"),
            "the always-on reference is below the fold before scrolling:\n{top}"
        );

        // Midway: the cursor reaches the first reference block (the fixed keys).
        // Step past the two header rows (prefix / preset) and every command.
        for _ in 0..crate::app::KEYS_HEADER_ROWS + crate::app::Cmd::ALL.len() {
            app.handle_settings_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        let mid = screen(&mut app);
        assert!(
            mid.contains("Always active") && mid.contains("focus panes"),
            "the always-on reference is reachable by cursor:\n{mid}"
        );

        // All the way down: Down keeps stepping through every reference row until
        // the last block (Mouse) is on screen — nothing is unreachable.
        for _ in 0..app.settings_rows(crate::app::SettingsTab::Keys) {
            app.handle_settings_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        let bottom = screen(&mut app);
        assert!(
            bottom.contains("Mouse") && bottom.contains("right-click"),
            "the last reference block (Mouse) is reachable:\n{bottom}"
        );
        // Copy & paste is its own labeled section (just above Mouse), so a user
        // looking for it can reach it even when reference row counts change.
        let mut reference_view = bottom;
        for _ in 0..app.settings_rows(crate::app::SettingsTab::Keys) {
            if reference_view.contains("Copy & paste") {
                break;
            }
            app.handle_settings_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
            reference_view = screen(&mut app);
        }
        assert!(
            reference_view.contains("Copy & paste"),
            "there is a Copy & paste reference block:\n{reference_view}"
        );

        // Copy mode's motions cannot be rebound, so the Keys tab is the only place
        // in the app that can teach them. A user who never reads the website has
        // to be able to find `e`, `^D`/`^U`, and the count prefix right here.
        let mut copy_view = reference_view;
        for _ in 0..app.settings_rows(crate::app::SettingsTab::Keys) {
            if copy_view.contains("w / e / B") {
                break;
            }
            app.handle_settings_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
            copy_view = screen(&mut app);
        }
        assert!(
            copy_view.contains("w / e / B")
                && copy_view.contains("^D / ^U")
                && copy_view.contains("12j moves twelve rows"),
            "copy mode's fixed motions are discoverable in the Keys tab:\n{copy_view}"
        );

        // The mouse wheel scrolls the list (moves the selection) without the arrows.
        app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 40,
            row: 12,
            modifiers: KeyModifiers::NONE,
        }));
        let after_up = app.settings.as_ref().map(|u| u.cursor).unwrap();
        app.handle_event(AppEvent::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 40,
            row: 12,
            modifiers: KeyModifiers::NONE,
        }));
        let after_down = app.settings.as_ref().map(|u| u.cursor).unwrap();
        assert!(
            after_down > after_up,
            "wheel down moves the selection further than wheel up ({after_up} -> {after_down})"
        );
    }

    /// End-to-end: start the socket server, run a mini app loop, and drive it
    /// over the wire like an agent would.
    #[test]
    fn api_serves_requests() {
        use std::io::{BufRead, BufReader, Write};

        let (tx, rx) = mpsc::channel();
        let mut app = App::new(80, 24, tx.clone()).unwrap();
        let path = std::env::temp_dir().join(format!("luvus-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let startup_lock =
            ipc::transport::acquire_server_startup_lock(path.parent().unwrap()).unwrap();
        let listener = ipc::api::bind_server(&path, &startup_lock).unwrap();
        ipc::api::start_server(listener, tx, app.events.clone());
        drop(startup_lock);
        thread::spawn(move || {
            while let Ok(ev) = rx.recv() {
                if let crate::event::AppEvent::Api(req) = ev {
                    let resp = app.handle_api(&req);
                    let _ = req.reply.send(resp);
                }
            }
        });

        let send = |req: &str| -> String {
            let mut s = ipc::transport::connect(&path).unwrap();
            writeln!(s, "{req}").unwrap();
            let mut line = String::new();
            BufReader::new(s).read_line(&mut line).unwrap();
            line
        };

        assert!(send(r#"{"id":"1","method":"ping","params":{}}"#).contains("pong"));
        let list = send(r#"{"id":"2","method":"pane.list","params":{}}"#);
        assert!(list.contains("pane_list"), "got: {list}");
        let split = send(r#"{"id":"3","method":"pane.split","params":{}}"#);
        assert!(split.contains("\"pane\""), "got: {split}");
        let _ = std::fs::remove_file(&path);
    }

    /// Render a representative frame (a simulated agent session in the pane) and
    /// dump it to `preview.html` so the UI can be viewed in a browser with real
    /// colors. Run with `cargo test --features dev-tools generate_preview`.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn generate_preview() {
        use crate::ui::theme::State;
        use ratatui::style::Modifier;

        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let key = |c, m| AppEvent::Key(KeyEvent::new(KeyCode::Char(c), m));

        let (tx, _rx) = mpsc::channel::<AppEvent>();
        let mut app = App::new(78, 30, tx).expect("spawn pane");

        // Split into two panes: Codex is the active working pane and Claude is
        // a second, independent agent. This is a mission-control scene.
        let left = app.layout().focus;
        app.handle_event(key(' ', KeyModifiers::CONTROL)); // prefix (Ctrl+Space)
        app.handle_event(key('v', KeyModifiers::NONE)); // split → side by side
        if let Some(p) = app.panes.get_mut(&left) {
            p.command = "codex".to_string();
        }

        // The blank row immediately above and below the Codex prompt matches
        // its live composer geometry, exercising Luvus's theme-aware composer
        // surface in both generated previews.
        let codex_payload = "\x1b[2J\x1b[H\r\n\
\x1b[1m  OpenAI Codex\x1b[0m  \x1b[38;5;245mv0.147.0\x1b[0m\r\n\
\x1b[38;5;245m  Codex · gpt-5.6-sol\x1b[0m\r\n\r\n\
\x1b[38;5;252m  Mapping workspace changes.\x1b[0m\r\n\r\n\
\x1b[38;5;114m  •\x1b[0m \x1b[38;5;252mExplored\x1b[0m  \x1b[38;5;111msrc/app\x1b[0m\r\n\
\x1b[38;5;114m  •\x1b[0m \x1b[38;5;252mEdited\x1b[0m  \x1b[38;5;111mpanes.rs\x1b[0m  \x1b[38;5;114m+22\x1b[0m\r\n\
\x1b[38;5;221m  •\x1b[0m \x1b[38;5;252mRan\x1b[0m  \x1b[38;5;114m529 passed\x1b[0m\r\n\r\n\
\x1b[38;5;245m  Ready to review.\x1b[0m\r\n\
\x1b[38;5;240m  ────────────────────────\x1b[0m\r\n\r\n\
\x1b[38;5;245m  ›\x1b[0m \x1b[38;5;252mSummarize commits\x1b[0m\r\n\x1b[A";
        if let Some(p) = app.panes.get(&left) {
            if let Ok(mut e) = p.engine.lock() {
                e.advance(codex_payload.as_bytes());
            }
        }

        // The second pane makes the sidebar's multi-agent view meaningful too.
        let right = app.layout().focus;
        if let Some(p) = app.panes.get_mut(&right) {
            p.command = "claude".to_string();
        }
        let prompt = "\x1b[2J\x1b[H\r\n\
\x1b[38;5;213m  ✻ Claude Code\x1b[0m  \x1b[38;5;245mopus-4.8\x1b[0m\r\n\r\n\
\x1b[38;5;252m  Reviewing release notes.\x1b[0m\r\n\r\n\
\x1b[38;5;114m  ●\x1b[0m \x1b[38;5;252mRead\x1b[0m  \x1b[38;5;111mv0.11.0.md\x1b[0m\r\n\
\x1b[38;5;221m  ●\x1b[0m \x1b[38;5;252mWorking\x1b[0m  \x1b[38;5;245mReady\x1b[0m\r\n\r\n\
\x1b[38;5;245m  >\x1b[0m \x1b[7m \x1b[0m";
        if let Some(p) = app.panes.get(&right) {
            if let Ok(mut e) = p.engine.lock() {
                e.advance(prompt.as_bytes());
            }
        }

        // Force representative states for the still image.
        if let Some(s) = app.status.get_mut(&left) {
            s.state = State::Working;
            s.agent = "codex".to_string();
        }
        if let Some(s) = app.status.get_mut(&right) {
            s.state = State::Working;
            s.agent = "claude".to_string();
        }
        assert!(
            app.panes
                .get(&left)
                .and_then(|p| p.engine.lock().ok())
                .and_then(|engine| engine.codex_composer_region())
                .is_some(),
            "the Codex fixture must retain the live composer geometry"
        );
        // Show the workspace with its git branch.
        app.workspaces[0].branch = Some("main".to_string());

        let backend = TestBackend::new(110, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();

        let (w, h) = (buf.area.width, buf.area.height);
        let mut body = String::new();
        for y in 0..h {
            for x in 0..w {
                let cell = &buf[(x, y)];
                let rev = cell.modifier.contains(Modifier::REVERSED);
                let mut fg = resolve(cell.fg, (0xcd, 0xd6, 0xf4));
                let mut bg = resolve(cell.bg, (0x1e, 0x1e, 0x2e));
                if rev {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if cell.modifier.contains(Modifier::DIM) {
                    fg = dim(fg);
                }
                let mut style = format!(
                    "color:#{:02x}{:02x}{:02x};background:#{:02x}{:02x}{:02x}",
                    fg.0, fg.1, fg.2, bg.0, bg.1, bg.2
                );
                if cell.modifier.contains(Modifier::BOLD) {
                    style.push_str(";font-weight:700");
                }
                if cell.modifier.contains(Modifier::ITALIC) {
                    style.push_str(";font-style:italic");
                }
                let sym = match cell.symbol() {
                    "" => " ",
                    s => s,
                };
                let esc = sym
                    .replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;");
                body.push_str(&format!("<span style=\"{style}\">{esc}</span>"));
            }
            body.push('\n');
        }

        let html = format!(
            "<!doctype html><meta charset=utf-8><title>luvus preview</title>\
<style>body{{background:#11111b;margin:0;padding:40px;display:flex;justify-content:center}}\
pre{{font:14px/1.3 'SF Mono',Menlo,Consolas,monospace;background:#1e1e2e;padding:0;\
border-radius:12px;overflow:hidden;box-shadow:0 16px 50px rgba(0,0,0,.6)}}\
span{{white-space:pre}}</style><pre>{body}</pre>"
        );
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/preview.html");
        std::fs::write(path, html).unwrap();
        eprintln!("wrote {path}");

        // ANSI truecolor version, viewable with `cat preview.ans`.
        let mut ans = String::new();
        for y in 0..h {
            for x in 0..w {
                let cell = &buf[(x, y)];
                let fg = resolve(cell.fg, (0xcd, 0xd6, 0xf4));
                let bg = resolve(cell.bg, (0x1e, 0x1e, 0x2e));
                ans.push_str(&format!(
                    "\x1b[38;2;{};{};{};48;2;{};{};{}m",
                    fg.0, fg.1, fg.2, bg.0, bg.1, bg.2
                ));
                if cell.modifier.contains(Modifier::BOLD) {
                    ans.push_str("\x1b[1m");
                }
                ans.push_str(match cell.symbol() {
                    "" => " ",
                    s => s,
                });
                ans.push_str("\x1b[0m");
            }
            ans.push('\n');
        }
        let apath = concat!(env!("CARGO_MANIFEST_DIR"), "/preview.ans");
        std::fs::write(apath, ans).unwrap();
        eprintln!("wrote {apath}");
    }

    #[cfg(feature = "dev-tools")]
    fn resolve(c: ratatui::style::Color, reset: (u8, u8, u8)) -> (u8, u8, u8) {
        use ratatui::style::Color::*;
        match c {
            Reset => reset,
            Rgb(r, g, b) => (r, g, b),
            Indexed(i) => xterm(i),
            Black => xterm(0),
            Red => xterm(1),
            Green => xterm(2),
            Yellow => xterm(3),
            Blue => xterm(4),
            Magenta => xterm(5),
            Cyan => xterm(6),
            Gray => xterm(7),
            DarkGray => xterm(8),
            LightRed => xterm(9),
            LightGreen => xterm(10),
            LightYellow => xterm(11),
            LightBlue => xterm(12),
            LightMagenta => xterm(13),
            LightCyan => xterm(14),
            White => xterm(15),
        }
    }

    #[cfg(feature = "dev-tools")]
    fn dim(c: (u8, u8, u8)) -> (u8, u8, u8) {
        let f = |v: u8| (v as f32 * 0.6) as u8;
        (f(c.0), f(c.1), f(c.2))
    }

    #[cfg(feature = "dev-tools")]
    fn xterm(i: u8) -> (u8, u8, u8) {
        // 0–15: catppuccin mocha ANSI; 16–231: 6×6×6 cube; 232–255: grayscale.
        const ANSI: [(u8, u8, u8); 16] = [
            (0x45, 0x47, 0x5a),
            (0xf3, 0x8b, 0xa8),
            (0xa6, 0xe3, 0xa1),
            (0xf9, 0xe2, 0xaf),
            (0x89, 0xb4, 0xfa),
            (0xf5, 0xc2, 0xe7),
            (0x94, 0xe2, 0xd5),
            (0xba, 0xc2, 0xde),
            (0x58, 0x5b, 0x70),
            (0xf3, 0x8b, 0xa8),
            (0xa6, 0xe3, 0xa1),
            (0xf9, 0xe2, 0xaf),
            (0x89, 0xb4, 0xfa),
            (0xf5, 0xc2, 0xe7),
            (0x94, 0xe2, 0xd5),
            (0xa6, 0xad, 0xc8),
        ];
        if i < 16 {
            ANSI[i as usize]
        } else if i < 232 {
            let i = i - 16;
            let c = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
            (c(i / 36), c((i / 6) % 6), c(i % 6))
        } else {
            let v = 8 + 10 * (i - 232);
            (v, v, v)
        }
    }

    /// Run with `cargo test --release --features dev-tools bench_file_viewer_render -- --nocapture`.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn bench_file_viewer_render() {
        use ratatui::{backend::TestBackend, Terminal};
        use std::time::Instant;
        let dir = std::env::temp_dir().join("luvus-perf-fv");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/app")).unwrap();
        std::fs::create_dir_all(dir.join("src/ui")).unwrap();
        for i in 0..40 {
            std::fs::write(dir.join(format!("src/f{i}.rs")), b"x").unwrap();
        }
        for i in 0..20 {
            std::fs::write(dir.join(format!("src/app/a{i}.rs")), b"x").unwrap();
        }
        let big: String = (1..=400)
            .map(|i| format!("line {i} of a source file with some length to it\n"))
            .collect();
        let file = dir.join("code.rs");
        std::fs::write(&file, big).unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(160, 50, tx).unwrap();
        app.workspaces[app.active_ws].cwd = dir.clone();
        app.sidebars.left.docks.push(crate::app::DockKind::Files);
        app.ensure_file_tree();
        app.file_tree
            .apply_dir(dir.clone(), crate::files::read_dir_entries(&dir));
        app.file_tree.toggle(&dir.join("src"));
        app.file_tree.apply_dir(
            dir.join("src"),
            crate::files::read_dir_entries(&dir.join("src")),
        );
        app.file_tree.toggle(&dir.join("src/app"));
        app.file_tree.apply_dir(
            dir.join("src/app"),
            crate::files::read_dir_entries(&dir.join("src/app")),
        );
        app.open_file_view(file.clone(), crate::app::files::OpenTarget::Pane);
        let vid = app.layout().focus;
        if let Some(crate::app::ViewKind::File(v)) = app.views.get_mut(&vid) {
            v.apply(crate::files::read_file(&file));
        }

        let rows = app.file_tree.visible_rows().len();
        let mut term = Terminal::new(TestBackend::new(160, 50)).unwrap();
        // warmup
        for _ in 0..20 {
            term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        }
        let n = 2000;
        let t = Instant::now();
        for _ in 0..n {
            term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        }
        let per = t.elapsed().as_nanos() as f64 / n as f64 / 1000.0;
        println!(
            "FVBENCH: {:.1}µs/frame  (files dock {} rows + 400-line view)",
            per, rows
        );

        // isolate visible_rows() cost
        let t = Instant::now();
        for _ in 0..n {
            let _ = app.file_tree.visible_rows();
        }
        let vr = t.elapsed().as_nanos() as f64 / n as f64 / 1000.0;
        println!(
            "FVBENCH: visible_rows() alone: {:.2}µs ({} rows, allocs a Vec+clones/call)",
            vr, rows
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End-to-end through the real render path: an edited file shows git change
    /// markers in the viewer's gutter (docs/38 + docs/30).
    #[test]
    fn file_viewer_shows_git_change_markers() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let _env = crate::persist::test_env("file-diff-markers");

        let dir = std::env::temp_dir().join(format!("luvus-fvdiff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sh = |args: &[&str]| {
            let _ = std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output();
        };
        sh(&["init", "-q"]);
        sh(&["config", "user.email", "t@t"]);
        sh(&["config", "user.name", "t"]);
        let file = dir.join("code.rs");
        std::fs::write(&file, "alpha\nbravo\ncharlie\n").unwrap();
        sh(&["add", "code.rs"]);
        sh(&["commit", "-qm", "base"]);
        std::fs::write(&file, "alpha\nBRAVO\ncharlie\ndelta\n").unwrap();

        let (tx, _rx) = mpsc::channel::<AppEvent>();
        let mut app = App::new(100, 20, tx).unwrap();
        app.open_file_view(file.clone(), crate::app::files::OpenTarget::Pane);
        let vid = app.layout().focus;
        if let Some(crate::app::ViewKind::File(v)) = app.views.get_mut(&vid) {
            v.apply(crate::files::read_file(&file));
            v.changes = crate::git::local::file_changes(&file);
            assert!(!v.changes.is_empty(), "the edit produced change spans");
        }

        let mut term = Terminal::new(TestBackend::new(100, 20)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let b = term.backend().buffer();
        // Find the marker column: the cell just left of the text on each row.
        let mut marks = 0usize;
        for y in 0..b.area.height {
            for x in 0..b.area.width {
                if b[(x, y)].symbol() == "▎" {
                    marks += 1;
                }
            }
        }
        assert!(marks > 0, "changed lines are marked in the gutter");

        // A clean file in the same repo shows no markers.
        let clean = dir.join("clean.rs");
        std::fs::write(&clean, "untouched\n").unwrap();
        sh(&["add", "clean.rs"]);
        sh(&["commit", "-qm", "clean"]);
        assert!(
            crate::git::local::file_changes(&clean).is_empty(),
            "a clean file has no markers"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
