//! The module command runner (docs/13 §3.3): builds the injected environment,
//! runs an argv command as a detached subprocess in the module root with
//! output capped at 64 KiB, and reports completion back to the loop via
//! `AppEvent::ModuleCommandFinished`. Fire-and-forget; the caller gets a
//! `Running` log immediately.

use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use super::paths;
use super::registry::InstalledModule;
use crate::event::AppEvent;

pub const MAX_IN_FLIGHT: usize = 32;
pub const LOG_LIMIT: usize = 200;
pub const OUTPUT_CAP: usize = 64 * 1024;
pub const SYNC_TIMEOUT: Duration = Duration::from_secs(300);
/// How long an action without a lifetime deadline may keep its pipes open
/// after its direct child exits. A descendant it started in the background can
/// inherit stdout/stderr/stdin; past this grace the run is reported complete
/// rather than left Running until that descendant happens to exit.
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Completed actions whose I/O threads are still attached because a background
/// descendant holds one of their pipes. They count toward [`MAX_IN_FLIGHT`] so
/// that leaving such a process running can never accumulate threads and pipe
/// handles without bound.
static LINGERING: AtomicUsize = AtomicUsize::new(0);

/// Completed actions still draining a pipe held open by a background process.
pub fn lingering_actions() -> usize {
    LINGERING.load(Ordering::Acquire)
}

/// Shared by one run's stdin writer and output readers. When a completed run is
/// detached from them, the last of those threads to finish releases its
/// [`LINGERING`] slot, however long the background process lives.
struct IoThreads {
    detached: AtomicBool,
}

impl Drop for IoThreads {
    fn drop(&mut self) {
        if self.detached.load(Ordering::Acquire) {
            LINGERING.fetch_sub(1, Ordering::AcqRel);
        }
    }
}
pub const MODULE_TOKEN_ENV: &str = "LUVUS_MODULE_TOKEN";

/// Commander always sends its versioned document, even when the action
/// declares no text input. Such an action may intentionally close stdin.
pub struct StdinRequest {
    pub bytes: Vec<u8>,
    pub allow_closed_on_success: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ModuleStatus {
    Running,
    Succeeded,
    Failed,
}

#[derive(Clone, Serialize)]
pub struct ModuleCommandLog {
    pub id: u64,
    pub module_id: String,
    /// What ran, e.g. `action:refresh` or `event:pane.agent_status_changed`.
    pub label: String,
    pub argv: Vec<String>,
    pub status: ModuleStatus,
    pub code: Option<i32>,
    pub out: String,
    pub err: String,
}

/// A process-wide monotonic id for command logs.
pub fn next_log_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The always-injected identity + context + settings environment (docs/13 §3.4).
/// Ensures the module's config/state dirs exist.
///
/// Alongside `LUVUS_MODULE_CONTEXT_JSON` this flattens the ids into plain
/// `LUVUS_WORKSPACE_ID` / `LUVUS_PANE_ID` / … vars and each declared setting
/// into `LUVUS_SETTING_<KEY>`, so a bash module never has to parse JSON.
fn base_env(module: &InstalledModule, module_token: &str, ctx: &Value) -> Vec<(String, String)> {
    let config = paths::config_dir(&module.id);
    let state = paths::state_dir(&module.id);
    let _ = std::fs::create_dir_all(&config);
    let _ = std::fs::create_dir_all(&state);

    let mut env = vec![
        ("LUVUS_ENV".to_string(), "1".to_string()),
        ("LUVUS_MODULE_ID".to_string(), module.id.clone()),
        (MODULE_TOKEN_ENV.to_string(), module_token.to_string()),
        (
            "LUVUS_MODULE_ROOT".to_string(),
            module.root.display().to_string(),
        ),
        (
            "LUVUS_MODULE_CONFIG_DIR".to_string(),
            config.display().to_string(),
        ),
        (
            "LUVUS_MODULE_STATE_DIR".to_string(),
            state.display().to_string(),
        ),
        ("LUVUS_MODULE_CONTEXT_JSON".to_string(), ctx.to_string()),
        (
            "LUVUS_MODULE_VERSION".to_string(),
            module.manifest.version.clone(),
        ),
    ];
    env.extend(super::context::env_from(ctx));
    env.extend(super::settings::env(&module.manifest, &module.id));
    if let Some(sock) = crate::ipc::api::socket_path_env() {
        env.push(("LUVUS_SOCKET_PATH".to_string(), sock));
    }
    if let Some(name) = crate::session::active_name() {
        env.push((crate::session::SESSION_ENV_VAR.to_string(), name));
    }
    if let Ok(exe) = std::env::current_exe() {
        env.push(("LUVUS_BIN_PATH".to_string(), exe.display().to_string()));
    }
    env
}

/// Build the complete module environment, including variables supplied by the
/// selected entrypoint, dock, row, action, or event.
pub fn env(
    module: &InstalledModule,
    module_token: &str,
    ctx: &Value,
    extra: Vec<(String, String)>,
) -> Vec<(String, String)> {
    complete_env(base_env(module, module_token, ctx), extra)
}

fn complete_env(
    mut base: Vec<(String, String)>,
    extra: Vec<(String, String)>,
) -> Vec<(String, String)> {
    base.extend(extra);
    base
}

/// Spawn `argv` in `root` on a detached thread; when it exits, send
/// `AppEvent::ModuleCommandFinished`. `argv` must be non-empty (manifest-validated).
pub fn spawn(
    log_id: u64,
    root: PathBuf,
    argv: Vec<String>,
    env: Vec<(String, String)>,
    app_tx: Sender<AppEvent>,
) {
    spawn_with_input(log_id, root, argv, env, None, app_tx);
}

/// Commander-only variant: write one bounded JSON document to stdin. Other
/// action entrypoints still receive a closed stdin and unchanged argv/env.
pub fn spawn_with_input(
    log_id: u64,
    root: PathBuf,
    argv: Vec<String>,
    env: Vec<(String, String)>,
    input: Option<StdinRequest>,
    app_tx: Sender<AppEvent>,
) {
    thread::spawn(move || {
        let allow_closed_on_success = input
            .as_ref()
            .is_some_and(|request| request.allow_closed_on_success);
        let input = input.map(|request| request.bytes);
        let (code, out, err) = run_with_input(
            &root,
            &argv,
            &env,
            input,
            None,
            None,
            allow_closed_on_success,
        );
        let _ = app_tx.send(AppEvent::ModuleCommandFinished {
            log_id,
            code,
            out,
            err,
        });
    });
}

/// Run one manifest-declared command synchronously for a core provider boundary.
/// The request is written as one JSON document to stdin. This uses the same
/// fixed argv, module cwd, identity, context, settings, output caps, and
/// no-window behavior as ordinary module commands.
pub fn run_sync(
    module: &InstalledModule,
    module_token: &str,
    ctx: &Value,
    argv: &[String],
    request: &Value,
    cancelled: &AtomicBool,
) -> Result<String, String> {
    let env = env(module, module_token, ctx, Vec::new());
    let (code, out, err) = run_with_input(
        &module.root,
        argv,
        &env,
        Some(request.to_string().into_bytes()),
        Some(SYNC_TIMEOUT),
        Some(cancelled),
        false,
    );
    if code == Some(0) {
        Ok(out)
    } else if err.trim().is_empty() {
        Err(match code {
            Some(code) => format!("module {} exited with code {code}", module.id),
            None => format!("module {} failed to run", module.id),
        })
    } else {
        Err(err.trim().to_string())
    }
}

pub(crate) fn run_bounded_argv(
    root: &PathBuf,
    argv: &[String],
    cancelled: &AtomicBool,
) -> Result<String, String> {
    run_bounded_argv_for(root, argv, cancelled, SYNC_TIMEOUT)
}

pub(crate) fn run_bounded_argv_for(
    root: &PathBuf,
    argv: &[String],
    cancelled: &AtomicBool,
    timeout: Duration,
) -> Result<String, String> {
    let env = Vec::new();
    let (code, out, err) = run(root, argv, &env, Some(timeout), Some(cancelled));
    if code == Some(0) {
        Ok(out)
    } else if err.trim().is_empty() {
        Err("command failed".to_string())
    } else {
        Err(err.trim().to_string())
    }
}

fn run(
    root: &PathBuf,
    argv: &[String],
    env: &[(String, String)],
    timeout: Option<Duration>,
    cancelled: Option<&AtomicBool>,
) -> (Option<i32>, String, String) {
    run_with_input(root, argv, env, None, timeout, cancelled, false)
}

fn run_with_input(
    root: &PathBuf,
    argv: &[String],
    env: &[(String, String)],
    input: Option<Vec<u8>>,
    timeout: Option<Duration>,
    cancelled: Option<&AtomicBool>,
    allow_closed_on_success: bool,
) -> (Option<i32>, String, String) {
    let Some((program, args)) = argv.split_first() else {
        return (None, String::new(), "empty command".to_string());
    };
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(root)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    crate::platform::no_window(&mut cmd);
    if timeout.is_some() {
        crate::platform::suspend_for_child_tree(&mut cmd);
    }
    #[cfg(unix)]
    if timeout.is_some() {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return (
                None,
                String::new(),
                format!("failed to spawn {program}: {e}"),
            )
        }
    };
    let mut tree_guard = if timeout.is_some() {
        match crate::platform::ChildTreeGuard::attach(&mut child) {
            Ok(guard) => Some(guard),
            Err(error) => {
                terminate_process_tree(child.id());
                let _ = child.kill();
                let _ = child.wait();
                return (
                    None,
                    String::new(),
                    format!("failed to contain module process tree: {error}"),
                );
            }
        }
    } else {
        None
    };
    let io_threads = Arc::new(IoThreads {
        detached: AtomicBool::new(false),
    });
    let mut stdin = child.stdin.take();
    let in_group = Arc::clone(&io_threads);
    let t_in = thread::spawn(move || -> std::io::Result<()> {
        let _group = in_group;
        if let (Some(stdin), Some(input)) = (stdin.as_mut(), input) {
            use std::io::Write;
            stdin.write_all(&input)?;
        }
        Ok(())
    });
    // Drain stdout + stderr concurrently (avoids a full-pipe deadlock), keeping
    // only the first OUTPUT_CAP bytes of each.
    let child_pid = child.id();
    let deadline = timeout.map(|duration| Instant::now() + duration);
    let mut so = child.stdout.take();
    let mut se = child.stderr.take();
    // The readers fill shared buffers rather than returning a String, so a run
    // whose pipes are still held open by a background descendant can report
    // what it captured without joining a reader that may never finish.
    let out_buf = Arc::new(Mutex::new(Vec::new()));
    let err_buf = Arc::new(Mutex::new(Vec::new()));
    let t_out = {
        let sink = Arc::clone(&out_buf);
        let group = Arc::clone(&io_threads);
        thread::spawn(move || {
            let _group = group;
            if let Some(r) = so.as_mut() {
                read_capped_into(r, &sink);
            }
        })
    };
    let t_err = {
        let sink = Arc::clone(&err_buf);
        let group = Arc::clone(&io_threads);
        thread::spawn(move || {
            let _group = group;
            if let Some(r) = se.as_mut() {
                read_capped_into(r, &sink);
            }
        })
    };
    let (status, process_timed_out, process_cancelled) =
        wait_for_child(&mut child, deadline, cancelled);
    let (output_timed_out, output_cancelled) =
        wait_for_output(&t_in, &t_out, &t_err, deadline, cancelled);
    let timed_out = process_timed_out || output_timed_out;
    let was_cancelled = process_cancelled || output_cancelled;
    if timed_out || was_cancelled {
        if let Some(guard) = &mut tree_guard {
            guard.terminate();
        }
        terminate_process_tree(child_pid);
        let _ = child.kill();
        let _ = child.wait();
        let grace = Instant::now() + Duration::from_secs(1);
        while (!t_in.is_finished() || !t_out.is_finished() || !t_err.is_finished())
            && Instant::now() < grace
        {
            thread::sleep(Duration::from_millis(10));
        }
        if !t_in.is_finished() || !t_out.is_finished() || !t_err.is_finished() {
            let message = if was_cancelled && !timed_out {
                "module command cancelled".to_string()
            } else {
                format!(
                    "module command timed out after {} seconds",
                    timeout.unwrap_or(SYNC_TIMEOUT).as_secs()
                )
            };
            return (None, String::new(), message);
        }
    }
    // Without a lifetime deadline the direct child has now exited. Bound only
    // the pipe drain: a descendant that inherited a handle must not keep the
    // run marked Running indefinitely. It is left running, because an action
    // may background work deliberately, and the child's real status is kept.
    let output_detached =
        deadline.is_none() && !drained_within(&t_in, &t_out, &t_err, OUTPUT_DRAIN_GRACE);
    if output_detached {
        // Held until the last I/O thread ends. This handle is dropped when the
        // function returns, so the slot cannot be released before it is taken.
        LINGERING.fetch_add(1, Ordering::AcqRel);
        io_threads.detached.store(true, Ordering::Release);
    }
    let input_result = if t_in.is_finished() {
        t_in.join()
            .unwrap_or_else(|_| Err(std::io::Error::other("module stdin writer panicked")))
    } else {
        // The action exited without reading all of its input, and a background
        // process still holds the read end. The input was not delivered, which
        // is the same outcome as a closed pipe, so it follows the same rule:
        // acceptable only for a successful action that declared no input.
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "the action exited before reading all of its input",
        ))
    };
    if !output_detached {
        let _ = t_out.join();
        let _ = t_err.join();
    }
    let out = captured(&out_buf);
    let mut err = captured(&err_buf);
    let closed_stdin_is_ok = allow_closed_on_success
        && input_result
            .as_ref()
            .is_err_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
        && status.as_ref().is_ok_and(|status| status.success());
    let input_failed = input_result.is_err() && !closed_stdin_is_ok;
    if let Err(error) = input_result {
        if !closed_stdin_is_ok {
            if !err.is_empty() && !err.ends_with('\n') {
                err.push('\n');
            }
            err.push_str(&format!("write stdin failed: {error}"));
        }
    }
    if output_detached {
        if !err.is_empty() && !err.ends_with('\n') {
            err.push('\n');
        }
        err.push_str(
            "a background process kept this action's output open after it exited; \
             later output was not captured, and the action counts toward the \
             in-flight command limit until that process closes it",
        );
    }
    drop(tree_guard);
    if timed_out || was_cancelled {
        if !err.is_empty() && !err.ends_with('\n') {
            err.push('\n');
        }
        if was_cancelled {
            err.push_str("module command cancelled");
        } else {
            err.push_str(&format!(
                "module command timed out after {} seconds",
                timeout.unwrap_or(SYNC_TIMEOUT).as_secs()
            ));
        }
    }
    match status {
        Ok(_) if timed_out || was_cancelled || input_failed => (None, out, err),
        Ok(s) => (s.code(), out, err),
        Err(e) => (None, out, format!("{err}\nwait failed: {e}")),
    }
}

fn wait_for_child(
    child: &mut std::process::Child,
    deadline: Option<Instant>,
    cancelled: Option<&AtomicBool>,
) -> (std::io::Result<std::process::ExitStatus>, bool, bool) {
    let Some(deadline) = deadline else {
        return (child.wait(), false, false);
    };
    loop {
        if cancelled.is_some_and(|cancelled| cancelled.load(Ordering::Acquire)) {
            terminate_process_tree(child.id());
            let _ = child.kill();
            return (child.wait(), false, true);
        }
        match child.try_wait() {
            Ok(Some(status)) => return (Ok(status), false, false),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                terminate_process_tree(child.id());
                let _ = child.kill();
                return (child.wait(), true, false);
            }
            Err(error) => return (Err(error), false, false),
        }
    }
}

fn wait_for_output(
    stdin: &thread::JoinHandle<std::io::Result<()>>,
    stdout: &thread::JoinHandle<()>,
    stderr: &thread::JoinHandle<()>,
    deadline: Option<Instant>,
    cancelled: Option<&AtomicBool>,
) -> (bool, bool) {
    let Some(deadline) = deadline else {
        return (false, false);
    };
    while !stdin.is_finished() || !stdout.is_finished() || !stderr.is_finished() {
        if cancelled.is_some_and(|cancelled| cancelled.load(Ordering::Acquire)) {
            return (false, true);
        }
        if Instant::now() >= deadline {
            return (true, false);
        }
        thread::sleep(Duration::from_millis(10));
    }
    (false, false)
}

fn terminate_process_tree(pid: u32) {
    #[cfg(unix)]
    unsafe {
        // Synchronous providers are spawned into a new process group above.
        let _ = libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = pid;
        // Timed children are suspended before spawn and attached to a
        // kill-on-close Job Object before resume. Dropping that guard is the
        // bounded native process-tree termination path.
    }
}

/// Read to EOF (so the child never blocks on a full pipe) but retain only the
/// first OUTPUT_CAP bytes, appended to `sink` as they arrive.
fn read_capped_into<R: Read>(r: &mut R, sink: &Mutex<Vec<u8>>) {
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let mut kept = sink.lock().unwrap_or_else(|e| e.into_inner());
                if kept.len() < OUTPUT_CAP {
                    let take = (OUTPUT_CAP - kept.len()).min(n);
                    kept.extend_from_slice(&chunk[..take]);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

/// The bytes a reader has captured so far, as text.
fn captured(buf: &Mutex<Vec<u8>>) -> String {
    let kept = buf.lock().unwrap_or_else(|e| e.into_inner());
    String::from_utf8_lossy(&kept).into_owned()
}

/// Wait up to `grace` for all three I/O threads; true when they all finished.
fn drained_within(
    stdin: &thread::JoinHandle<std::io::Result<()>>,
    stdout: &thread::JoinHandle<()>,
    stderr: &thread::JoinHandle<()>,
    grace: Duration,
) -> bool {
    let until = Instant::now() + grace;
    loop {
        if stdin.is_finished() && stdout.is_finished() && stderr.is_finished() {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::{complete_env, MODULE_TOKEN_ENV};
    #[cfg(unix)]
    use super::{lingering_actions, run, run_with_input, AtomicBool, Duration, Ordering};

    #[cfg(unix)]
    #[test]
    fn sync_provider_writes_json_to_stdin() {
        let root = std::env::temp_dir();
        let (code, out, err) = run_with_input(
            &root,
            &["/bin/sh".into(), "-c".into(), "cat".into()],
            &[],
            Some(br#"{"version":1,"operation":"create"}"#.to_vec()),
            Some(Duration::from_secs(2)),
            None,
            false,
        );
        assert_eq!(code, Some(0), "{err}");
        assert_eq!(out, r#"{"version":1,"operation":"create"}"#);
    }

    #[cfg(unix)]
    #[test]
    fn sync_provider_rejects_a_truncated_stdin_request_even_after_exit_zero() {
        let root = std::env::temp_dir();
        let (code, _out, err) = run_with_input(
            &root,
            &[
                "/bin/sh".into(),
                "-c".into(),
                "exec 0>&-; printf '{\"path\":\"/unused\"}'".into(),
            ],
            &[],
            Some(vec![b'x'; 1024 * 1024]),
            Some(Duration::from_secs(2)),
            None,
            false,
        );
        assert_eq!(code, None);
        assert!(err.contains("write stdin failed"), "{err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn commander_no_text_action_can_close_stdin_and_exit_successfully() {
        let root = std::env::temp_dir();
        let (code, _out, err) = run_with_input(
            &root,
            &[
                "/bin/sh".into(),
                "-c".into(),
                "exec 0>&-; sleep 0.05; exit 0".into(),
            ],
            &[],
            Some(vec![b'x'; 1024 * 1024]),
            None,
            None,
            true,
        );
        assert_eq!(code, Some(0), "{err}");
        assert!(err.is_empty(), "{err}");
    }

    /// Runs that leave I/O threads attached share one global count, so every
    /// test that creates one holds this lock and waits for its own count to
    /// drain before releasing it. The baseline each test sees is then exact.
    #[cfg(unix)]
    static LINGER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// POSIX shells to run these tests under. `/bin/sh` is bash on macOS but
    /// dash on Debian-based CI, and the two differ in how a background job
    /// inherits stdin, so dash is exercised too wherever it is installed.
    #[cfg(unix)]
    fn test_shells() -> Vec<&'static str> {
        ["/bin/sh", "/bin/dash"]
            .into_iter()
            .filter(|shell| std::path::Path::new(shell).exists())
            .collect()
    }

    /// Run `script` (which must write its background PID to `$BG_PID_FILE`)
    /// with `input` on stdin, then return the result, the background PID, and
    /// the lingering count observed right after the run completed.
    #[cfg(unix)]
    fn run_with_background(
        shell: &str,
        name: &str,
        script: &str,
        input: Vec<u8>,
        allow_closed_on_success: bool,
    ) -> ((Option<i32>, String, String), i32, usize, Duration) {
        let dir = std::env::temp_dir().join(format!("luvus-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("bg.pid");
        let started = std::time::Instant::now();
        let result = run_with_input(
            &dir,
            &[shell.into(), "-c".into(), script.into()],
            &[("BG_PID_FILE".into(), pid_file.display().to_string())],
            Some(input),
            None,
            None,
            allow_closed_on_success,
        );
        let elapsed = started.elapsed();
        let lingering = lingering_actions();
        let bg: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        (result, bg, lingering, elapsed)
    }

    /// Stop a background process and wait until the lingering count is back
    /// to `baseline`, proving its I/O threads released their slot.
    #[cfg(unix)]
    fn stop_and_drain(bg: i32, baseline: usize) {
        unsafe {
            libc::kill(bg, libc::SIGKILL);
        }
        let until = std::time::Instant::now() + Duration::from_secs(10);
        while lingering_actions() != baseline && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            lingering_actions(),
            baseline,
            "a finished background process did not release its I/O threads"
        );
    }

    /// A Commander action that exits while a background descendant still holds
    /// its stdout must complete promptly with the child's real exit status and
    /// the output captured so far, and must leave that descendant running. Its
    /// still-attached readers hold one in-flight slot until the descendant
    /// exits, and release it afterwards.
    #[cfg(unix)]
    #[test]
    fn async_action_completes_when_a_descendant_keeps_output_open() {
        let _lock = LINGER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for shell in test_shells() {
            let baseline = lingering_actions();
            let ((code, out, err), bg, lingering, elapsed) = run_with_background(
                shell,
                "drain",
                "sleep 30 & echo $! > \"$BG_PID_FILE\"; printf ready",
                br#"{"version":1}"#.to_vec(),
                true,
            );
            let still_running = unsafe { libc::kill(bg, 0) } == 0;
            stop_and_drain(bg, baseline);

            assert!(
                elapsed < Duration::from_secs(10),
                "{shell}: blocked {elapsed:?}"
            );
            assert_eq!(code, Some(0), "{shell}: the action succeeded: {err:?}");
            assert_eq!(out, "ready", "{shell}");
            assert!(err.contains("background process kept"), "{shell}: {err:?}");
            assert!(still_running, "{shell}: a backgrounded process was killed");
            assert_eq!(lingering, baseline + 1, "{shell}: readers hold a slot");
        }
    }

    /// Keeps the action's stdin open in a background process that never reads
    /// it. A plain `&` gives a background job `/dev/null` as stdin, and dash
    /// applies that before an explicit `<&0`, so stdin is saved to fd 3 first.
    #[cfg(unix)]
    const HOLD_STDIN_UNREAD: &str = "exec 3<&0; sleep 30 <&3 & echo $! > \"$BG_PID_FILE\"; exit 0";

    /// Input the action never read was not delivered. For an action that reads
    /// its input that is a failure, even though the action exited with 0 and a
    /// background process is still holding stdin.
    #[cfg(unix)]
    #[test]
    fn async_action_that_leaves_input_unread_is_not_reported_as_delivered() {
        let _lock = LINGER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for shell in test_shells() {
            let baseline = lingering_actions();
            // Larger than any pipe buffer, so the write cannot complete unread.
            let ((code, _out, err), bg, _, elapsed) = run_with_background(
                shell,
                "unread-input",
                HOLD_STDIN_UNREAD,
                vec![b'x'; 1 << 20],
                false,
            );
            stop_and_drain(bg, baseline);

            assert!(
                elapsed < Duration::from_secs(10),
                "{shell}: blocked {elapsed:?}"
            );
            assert_eq!(
                code, None,
                "{shell}: unread input reported success: {err:?}"
            );
            assert!(
                err.contains("before reading all of its input"),
                "{shell}: {err:?}"
            );
        }
    }

    /// The same unread input is fine for an action that declared it takes no
    /// input, matching how a closed stdin is already treated.
    #[cfg(unix)]
    #[test]
    fn async_action_without_input_may_leave_stdin_unread() {
        let _lock = LINGER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for shell in test_shells() {
            let baseline = lingering_actions();
            let ((code, _out, err), bg, _, _) = run_with_background(
                shell,
                "unread-none",
                HOLD_STDIN_UNREAD,
                vec![b'x'; 1 << 20],
                true,
            );
            stop_and_drain(bg, baseline);

            assert_eq!(code, Some(0), "{shell}: {err:?}");
            assert!(!err.contains("before reading all"), "{shell}: {err:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn sync_cancellation_kills_descendants_holding_output_pipes() {
        let root = std::env::temp_dir();
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let signal = cancelled.clone();
        let trigger = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            signal.store(true, Ordering::Release);
        });
        let started = std::time::Instant::now();
        let (code, _out, err) = run(
            &root,
            &[
                "/bin/sh".into(),
                "-c".into(),
                "sleep 60 & printf ready; wait".into(),
            ],
            &[],
            Some(Duration::from_secs(60)),
            Some(&cancelled),
        );
        trigger.join().unwrap();
        assert_eq!(code, None);
        assert!(err.contains("cancelled"), "{err:?}");
        assert!(!err.contains("timed out"), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[cfg(unix)]
    #[test]
    fn sync_timeout_kills_descendants_holding_output_pipes() {
        let root = std::env::temp_dir();
        let started = std::time::Instant::now();
        let (code, _out, err) = run(
            &root,
            &[
                "/bin/sh".into(),
                "-c".into(),
                "sleep 60 & printf ready".into(),
            ],
            &[],
            Some(Duration::from_millis(100)),
            None,
        );
        assert_eq!(code, None);
        assert!(err.contains("timed out"), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn complete_environment_keeps_canonical_module_variables() {
        let env = complete_env(
            vec![
                ("LUVUS_MODULE_ID".into(), "example.test".into()),
                (MODULE_TOKEN_ENV.into(), "runtime-token".into()),
            ],
            vec![
                ("LUVUS_MODULE_ENTRYPOINT_ID".into(), "monitor".into()),
                ("LUVUS_MODULE_DOCK_ID".into(), "boards".into()),
                ("LUVUS_MODULE_ACTION_ID".into(), "flash".into()),
            ],
        );
        for (key, value) in [
            ("LUVUS_MODULE_ID", "example.test"),
            (MODULE_TOKEN_ENV, "runtime-token"),
            ("LUVUS_MODULE_ENTRYPOINT_ID", "monitor"),
            ("LUVUS_MODULE_DOCK_ID", "boards"),
            ("LUVUS_MODULE_ACTION_ID", "flash"),
        ] {
            assert!(
                env.contains(&(key.to_string(), value.to_string())),
                "missing {key}"
            );
        }
    }
}
