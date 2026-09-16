//! The module command runner (docs/13 §3.3): builds the injected environment,
//! runs an argv command as a detached subprocess in the module root with
//! output capped at 64 KiB, and reports completion back to the loop via
//! `AppEvent::ModuleCommandFinished`. Fire-and-forget; the caller gets a
//! `Running` log immediately.

use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
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
pub const MODULE_TOKEN_ENV: &str = "LUVUS_MODULE_TOKEN";

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
    thread::spawn(move || {
        let (code, out, err) = run(&root, &argv, &env, None, None);
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
    run_with_input(root, argv, env, None, timeout, cancelled)
}

fn run_with_input(
    root: &PathBuf,
    argv: &[String],
    env: &[(String, String)],
    input: Option<Vec<u8>>,
    timeout: Option<Duration>,
    cancelled: Option<&AtomicBool>,
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
    let mut stdin = child.stdin.take();
    let t_in = thread::spawn(move || -> std::io::Result<()> {
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
    let t_out = thread::spawn(move || so.as_mut().map(read_capped).unwrap_or_default());
    let t_err = thread::spawn(move || se.as_mut().map(read_capped).unwrap_or_default());
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
    let input_result = t_in
        .join()
        .unwrap_or_else(|_| Err(std::io::Error::other("module stdin writer panicked")));
    let out = t_out.join().unwrap_or_default();
    let mut err = t_err.join().unwrap_or_default();
    let input_failed = input_result.is_err();
    if let Err(error) = input_result {
        if !err.is_empty() && !err.ends_with('\n') {
            err.push('\n');
        }
        err.push_str(&format!("write stdin failed: {error}"));
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
    stdout: &thread::JoinHandle<String>,
    stderr: &thread::JoinHandle<String>,
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
/// first OUTPUT_CAP bytes.
fn read_capped<R: Read>(r: &mut R) -> String {
    let mut kept = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if kept.len() < OUTPUT_CAP {
                    let take = (OUTPUT_CAP - kept.len()).min(n);
                    kept.extend_from_slice(&chunk[..take]);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&kept).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{complete_env, MODULE_TOKEN_ENV};
    #[cfg(unix)]
    use super::{run, run_with_input, AtomicBool, Duration, Ordering};

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
        );
        assert_eq!(code, None);
        assert!(err.contains("write stdin failed"), "{err:?}");
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
