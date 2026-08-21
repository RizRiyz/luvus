//! luvus — mission control for your AI coding agents.
//! A client/server terminal workspace with live agent detection.
//! See docs/12-execution-plan.md.

mod agent;
mod app;
mod changelog;
mod cli;
mod compat;
mod config;
mod detect;
mod event;
mod files;
mod git;
mod i18n;
mod ids;
mod integration;
mod ipc;
mod layout;
mod links;
mod mission;
mod module;
mod orch;
mod persist;
mod platform;
mod session;
mod skill;
mod terminal;
mod ui;
mod update;

use std::io::{BufRead, BufReader, Write};
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

fn main() -> Result<()> {
    // Run the whole process at 1ms timer resolution so the event loop's timed
    // waits aren't quantized to Windows' ~15.6ms default (the cause of laggy
    // typing in panes there). No-op on Unix; restored when `main` returns.
    let _timer = platform::high_res_timer();
    compat::normalize_legacy_environment();
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

    // Migration belongs to commands that use the runtime, not informational
    // output. In particular, `luvus --version` must stay side-effect-free and
    // must never contaminate stdout/stderr used by installers and scripts.
    persist::migrate_legacy_state()?;
    integration::migrate_legacy_integrations();
    // One-time local cleanup of the old default-on skill installation. This
    // never downloads or installs a skill; it only removes Luvus/Bohay-managed
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
        Some("integration") => std::process::exit(integration::run(&args)?),
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

/// Play a short retro "done" jingle when an agent finishes. Runs client-side,
/// like `emit_notification`. The WAV is synthesized once and cached in the temp
/// dir, then played with the platform's audio tool in a detached thread so it
/// never blocks the event loop. A silent no-op if no player is available.
pub(crate) fn emit_sound() {
    std::thread::spawn(|| {
        if let Some(path) = ensure_done_jingle() {
            play_sound_file(&path);
        }
    });
}

/// Synthesize the jingle WAV to `<tmp>/luvus-done.wav` on first use; return it.
fn ensure_done_jingle() -> Option<std::path::PathBuf> {
    let path = std::env::temp_dir().join("luvus-done.wav");
    if !path.exists() {
        std::fs::write(&path, synth_done_wav()).ok()?;
    }
    Some(path)
}

/// A tiny 8-bit-style "level up" flourish: four ascending square-wave notes
/// (C-E-G-C), the last held a beat longer, with short fades so edges don't click.
fn synth_done_wav() -> Vec<u8> {
    const SR: u32 = 22_050;
    let notes = [523.25f32, 659.25, 783.99, 1046.5]; // C5 E5 G5 C6
    let amp = 0.22f32;
    let mut samples: Vec<i16> = Vec::new();
    for (i, &freq) in notes.iter().enumerate() {
        let base = SR * 90 / 1000; // 90 ms
        let n = if i + 1 == notes.len() { base * 2 } else { base };
        let fade = (SR / 200).max(1); // ~5 ms
        for s in 0..n {
            let phase = (s as f32 * freq / SR as f32) % 1.0;
            let sq = if phase < 0.5 { amp } else { -amp };
            let up = s.min(fade) as f32 / fade as f32;
            let down = n.saturating_sub(s).min(fade) as f32 / fade as f32;
            let env = up.min(down);
            samples.push((sq * env * i16::MAX as f32) as i16);
        }
    }
    wav_bytes(&samples, SR)
}

/// Wrap 16-bit mono PCM in a minimal WAV container.
fn wav_bytes(samples: &[i16], sr: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut v = Vec::with_capacity(44 + data_len as usize);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_len).to_le_bytes());
    v.extend_from_slice(b"WAVE");
    v.extend_from_slice(b"fmt ");
    v.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    v.extend_from_slice(&1u16.to_le_bytes()); // format = PCM
    v.extend_from_slice(&1u16.to_le_bytes()); // channels = mono
    v.extend_from_slice(&sr.to_le_bytes()); // sample rate
    v.extend_from_slice(&(sr * 2).to_le_bytes()); // byte rate
    v.extend_from_slice(&2u16.to_le_bytes()); // block align
    v.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

/// Play a WAV with the platform's audio tool (blocking — called in a thread).
fn play_sound_file(path: &Path) {
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
    let owned = path.to_string_lossy().into_owned();
    let p = owned.as_str();
    #[cfg(target_os = "macos")]
    {
        let _ = run("afplay", &[p]);
    }
    #[cfg(target_os = "windows")]
    {
        let script = format!("(New-Object Media.SoundPlayer '{p}').PlaySync()");
        let _ = run("powershell", &["-NoProfile", "-Command", &script]);
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
    result
}

fn autodetect_and_attach() -> Result<()> {
    let sock = persist::client_socket_path();
    let fresh = !server_running(&sock);
    if fresh {
        spawn_server()?;
        wait_for_socket(&sock)?;
    }
    if !fresh {
        // An upgraded binary silently attaching to an older running server means
        // none of the new version shows up — tell the user how to load it (the
        // brief pause keeps the note readable before the UI takes the screen).
        let binary = env!("CARGO_PKG_VERSION");
        if let Some(running) = server_version().filter(|running| running != binary) {
            eprintln!(
                "luvus v{binary} installed, but the running server is v{running} — \
                 run `luvus server restart` to load it (your session is saved and restored)."
            );
            thread::sleep(Duration::from_millis(2000));
        }
    }
    // Always ask the server to open the launch folder. A *fresh* server may have
    // restored a saved session (`restore_or_new`), in which case it never saw
    // this cwd — so this cannot be skipped on the fresh path. Idempotent: if the
    // folder is already a workspace, the server just focuses it.
    open_cwd_workspace();
    ipc::client::run(&sock)
}

/// Ask the running server to open the current directory as a workspace (add +
/// focus if new). Best-effort — a failure just means no auto-open.
fn open_cwd_workspace() {
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    let Ok(mut s) = ipc::transport::connect(&persist::socket_path()) else {
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
    let mut line = String::new();
    let _ = BufReader::new(s).read_line(&mut line); // wait for the ack before attaching
}

/// Remote bridge role (docs/18 RA-1), run *on the remote host* by ssh. Ensure a
/// server is up, then pump this process's stdin/stdout to/from the local socket
/// so the `luvus --remote` client on the other end of the ssh pipe drives it.
fn remote_client_bridge() -> Result<()> {
    let sock = persist::client_socket_path();
    if !server_running(&sock) {
        spawn_server()?;
        wait_for_socket(&sock)?;
    }
    ipc::client::remote_bridge(&sock)
}

/// `luvus attach <id>` (docs/18 WA-2): focus + zoom the pane (one round-trip via
/// `attach.pane`), then attach the client so it opens straight into that
/// fullscreen terminal. Composes with `--remote` for a remote fullscreen attach.
fn attach_cmd(args: &[String]) -> Result<()> {
    let sock = persist::client_socket_path();
    if !server_running(&sock) {
        spawn_server()?;
        wait_for_socket(&sock)?;
    }
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
    ipc::transport::connect(sock).is_ok()
}

fn spawn_server() -> Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("server")
        // The selector was already resolved into LUVUS_SESSION. A parent pane's
        // injected API socket must not leak into a newly spawned server.
        .env_remove("LUVUS_SOCKET_PATH")
        .env_remove("BOHAY_SOCKET_PATH")
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
    match args.get(2).map(String::as_str) {
        None => ipc::server::run(), // internal role: run the server in the foreground
        Some("start") => server_start(),
        Some("stop") => server_stop(),
        Some("restart") => server_restart(),
        Some("status") => server_status(),
        Some("update-manifest") => update_manifest(),
        Some(other) => {
            eprintln!("unknown server command: {other}");
            eprintln!("usage: luvus server <start|stop|restart|status|update-manifest>");
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
fn update_manifest() -> Result<()> {
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
            eprintln!("skipping suspicious manifest name: {name}");
            skipped += 1;
            continue;
        }
        let body = match crate::module::discovery::http_get(&format!("{base}/{name}")) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skipping {name}: {e}");
                skipped += 1;
                continue;
            }
        };
        // Reject a garbled download before it can land in the managed dir.
        if !crate::detect::manifest_parses(&body) {
            eprintln!("skipping {name}: not a valid detection manifest");
            skipped += 1;
            continue;
        }
        std::fs::write(managed.join(name), body.as_bytes())
            .map_err(|e| anyhow!("cannot write {name}: {e}"))?;
        written += 1;
    }
    println!(
        "updated {written} detection manifest(s){} -> {}",
        if skipped > 0 {
            format!(", {skipped} skipped")
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
            println!("reloaded into the running server ({n} rules active) - no restart needed");
        }
        Err(_) => println!("no server running - the update loads on next start"),
    }
    Ok(())
}

/// Spawn the detached server if one isn't already up.
fn server_start() -> Result<()> {
    let sock = persist::client_socket_path();
    if server_running(&sock) {
        println!(
            "luvus server already running (session {})",
            session::display_name()
        );
        return Ok(());
    }
    spawn_server()?;
    wait_for_socket(&sock)?;
    println!("luvus server started (session {})", session::display_name());
    Ok(())
}

fn server_stop() -> Result<()> {
    let sock = persist::client_socket_path();
    if send_server_stop() {
        // The server acks before it actually exits, so wait for it to release the
        // socket — then `stop` returning means it's really down (and a following
        // `status` reports "not running", not a half-shutdown "running").
        wait_for_shutdown(&sock);
        println!("luvus server stopped (session {})", session::display_name());
    } else {
        println!("no luvus server running");
    }
    Ok(())
}

/// Stop (if running), wait for the socket to close, then start a fresh server —
/// the way to load a newly-installed binary without rebooting a live session.
fn server_restart() -> Result<()> {
    let sock = persist::client_socket_path();
    if send_server_stop() {
        wait_for_shutdown(&sock);
    }
    spawn_server()?;
    wait_for_socket(&sock)?;
    println!(
        "luvus server restarted (session {})",
        session::display_name()
    );
    Ok(())
}

/// Poll (bounded) until the server releases its socket, so `stop`/`restart`
/// return only once the old server is truly gone.
fn wait_for_shutdown(sock: &Path) {
    for _ in 0..100 {
        if !server_running(sock) {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Report whether a server is up and, if so, the version it's *running* — which
/// can differ from this binary when a new install hasn't been restarted yet.
fn server_status() -> Result<()> {
    let sock = persist::client_socket_path();
    if !server_running(&sock) {
        println!(
            "luvus server: not running (session {})",
            session::display_name()
        );
        return Ok(());
    }
    match server_version() {
        Some(running) => {
            println!(
                "luvus server: running (v{running}, session {})",
                session::display_name()
            );
            let binary = env!("CARGO_PKG_VERSION");
            if running != binary {
                println!(
                    "  note: this binary is v{binary} — run `luvus server restart` to load it"
                );
            }
        }
        None => println!(
            "luvus server: running (session {})",
            session::display_name()
        ),
    }
    Ok(())
}

/// Send `server.stop` to a running server; returns whether one answered.
fn send_server_stop() -> bool {
    match ipc::transport::connect(&persist::socket_path()) {
        Ok(mut s) => {
            let _ = writeln!(s, r#"{{"id":"1","method":"server.stop","params":{{}}}}"#);
            // Read the ack so the server has processed the request before we return.
            let mut line = String::new();
            let _ = BufReader::new(s).read_line(&mut line);
            true
        }
        Err(_) => false,
    }
}

/// Ask the running server its version via `ping`. `None` if unreachable/unparsable.
fn server_version() -> Option<String> {
    let mut s = ipc::transport::connect(&persist::socket_path()).ok()?;
    writeln!(s, r#"{{"id":"1","method":"ping","params":{{}}}}"#).ok()?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).ok()?;
    let v: serde_json::Value = serde_json::from_str(&line).ok()?;
    v.get("result")?.get("version")?.as_str().map(String::from)
}

fn run(terminal: &mut DefaultTerminal) -> Result<()> {
    let (tx, rx) = mpsc::channel::<AppEvent>();

    let size = terminal.size()?;
    // Rough initial PTY size; the first draw resizes it to the exact pane rect.
    let cols = size.width.saturating_sub(34).max(20);
    let rows = size.height.saturating_sub(4).max(4);

    // `--local` still exposes the control API, so it must obey the same
    // single-server ownership rules as the headless server.
    let state_dir = persist::ensure_session_dir();
    let startup_lock = ipc::transport::acquire_server_startup_lock(&state_dir)?;
    let sock = persist::socket_path();
    let client_sock = persist::client_socket_path();
    if ipc::transport::connect(&sock).is_ok() || ipc::transport::connect(&client_sock).is_ok() {
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
        if app.should_quit || app.detach_requested {
            break;
        }

        // Debounced session save.
        if app.session_dirty && last_save.elapsed() > Duration::from_secs(2) {
            persist::save(&app);
            app.session_dirty = false;
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
        if app.pending_sound {
            app.pending_sound = false;
            emit_sound();
        }
        // Advance the working spinner ~10x/s (the loop redraws every frame).
        if last_spin.elapsed() >= Duration::from_millis(100) && app.any_working() {
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

    persist::save(&app);
    Ok(())
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

    // The synthesized "done" jingle is a well-formed 16-bit mono WAV.
    #[test]
    fn done_jingle_is_valid_wav() {
        let w = synth_done_wav();
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(&w[8..12], b"WAVE");
        assert_eq!(&w[12..16], b"fmt ");
        assert_eq!(&w[36..40], b"data");
        let data_len = u32::from_le_bytes(w[40..44].try_into().unwrap()) as usize;
        assert_eq!(w.len(), 44 + data_len, "header data length matches payload");
        assert!(data_len > 0, "non-empty audio");
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
        let mut last = frame_from_buffer(term.backend().buffer(), None);

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
        let frame = frame_from_buffer(&owned, None);
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
        assert!(text.contains("Wire the auth module"), "task title missing");
        assert!(text.contains("claimed"), "task status missing");
        assert!(text.contains("LEASES"), "leases section missing");
        assert!(text.contains("◇ orch"), "board tab label missing");
    }

    /// The board's UX layer renders: a Running worker row with its live agent
    /// state, the start-worker picker, and the task detail overlay.
    #[test]
    fn renders_board_live_state_picker_and_detail() {
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
        // The worker pane's live detection state rides on the row.
        if let Some(st) = app.status.get_mut(&pane) {
            st.agent = "claude".into();
            st.state = crate::ui::theme::State::Working;
        }
        app.open_orch_board();

        let render_text = |app: &mut App| {
            let mut terminal = Terminal::new(TestBackend::new(110, 32)).unwrap();
            terminal.draw(|f| ui::render(f, app)).unwrap();
            let buf = terminal.backend().buffer().clone();
            buf.content().iter().map(|c| c.symbol()).collect::<String>()
        };

        let text = render_text(&mut app);
        assert!(text.contains("running"), "started worker shows running");
        assert!(text.contains("luvus/t1"), "worker branch shown");
        assert!(text.contains("working"), "live agent state shown");

        // The start-worker picker draws over the board.
        app.orch_start = Some(crate::app::OrchStart {
            task: "t1".into(),
            cursor: 0,
        });
        let text = render_text(&mut app);
        assert!(text.contains("claude"), "picker lists agents");
        app.orch_start = None;

        // The detail overlay shows the task's binding.
        app.orch_detail = Some("t1".into());
        let text = render_text(&mut app);
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
            !top.contains("not rebindable"),
            "the always-on reference is below the fold before scrolling:\n{top}"
        );

        // Midway: the cursor reaches the first reference block (the fixed keys).
        // Step past the two header rows (prefix / preset) and every command.
        for _ in 0..crate::app::KEYS_HEADER_ROWS + crate::app::Cmd::ALL.len() {
            app.handle_settings_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        let mid = screen(&mut app);
        assert!(
            mid.contains("not rebindable") && mid.contains("focus panes"),
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
        // looking for it finds it — scroll up a little from the bottom.
        for _ in 0..8 {
            app.handle_settings_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        }
        assert!(
            screen(&mut app).contains("Copy & paste"),
            "there is a Copy & paste reference block"
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
