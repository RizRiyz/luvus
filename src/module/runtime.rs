//! The module command runner (docs/13 §3.3): builds the injected environment,
//! runs an argv command as a detached subprocess in the module root with
//! output capped at 64 KiB, and reports completion back to the loop via
//! `AppEvent::ModuleCommandFinished`. Fire-and-forget; the caller gets a
//! `Running` log immediately.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::thread;

use serde::Serialize;
use serde_json::Value;

use super::paths;
use super::registry::InstalledModule;
use crate::event::AppEvent;

pub const MAX_IN_FLIGHT: usize = 32;
pub const LOG_LIMIT: usize = 200;
pub const OUTPUT_CAP: usize = 64 * 1024;

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
fn base_env(module: &InstalledModule, ctx: &Value) -> Vec<(String, String)> {
    let config = paths::config_dir(&module.id);
    let state = paths::state_dir(&module.id);
    let _ = std::fs::create_dir_all(&config);
    let _ = std::fs::create_dir_all(&state);

    let mut env = vec![
        ("LUVUS_ENV".to_string(), "1".to_string()),
        ("LUVUS_MODULE_ID".to_string(), module.id.clone()),
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
    ctx: &Value,
    extra: Vec<(String, String)>,
) -> Vec<(String, String)> {
    complete_env(base_env(module, ctx), extra)
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
        let (code, out, err) = run(&root, &argv, &env);
        let _ = app_tx.send(AppEvent::ModuleCommandFinished {
            log_id,
            code,
            out,
            err,
        });
    });
}

fn run(root: &PathBuf, argv: &[String], env: &[(String, String)]) -> (Option<i32>, String, String) {
    let Some((program, args)) = argv.split_first() else {
        return (None, String::new(), "empty command".to_string());
    };
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    crate::platform::no_window(&mut cmd);
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
    // Drain stdout + stderr concurrently (avoids a full-pipe deadlock), keeping
    // only the first OUTPUT_CAP bytes of each.
    let mut so = child.stdout.take();
    let mut se = child.stderr.take();
    let t_out = thread::spawn(move || so.as_mut().map(read_capped).unwrap_or_default());
    let t_err = thread::spawn(move || se.as_mut().map(read_capped).unwrap_or_default());
    let status = child.wait();
    let out = t_out.join().unwrap_or_default();
    let err = t_err.join().unwrap_or_default();
    match status {
        Ok(s) => (s.code(), out, err),
        Err(e) => (None, out, format!("{err}\nwait failed: {e}")),
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
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&kept).into_owned()
}

#[cfg(test)]
mod tests {
    use super::complete_env;

    #[test]
    fn complete_environment_keeps_canonical_module_variables() {
        let env = complete_env(
            vec![("LUVUS_MODULE_ID".into(), "example.test".into())],
            vec![
                ("LUVUS_MODULE_ENTRYPOINT_ID".into(), "monitor".into()),
                ("LUVUS_MODULE_DOCK_ID".into(), "boards".into()),
                ("LUVUS_MODULE_ACTION_ID".into(), "flash".into()),
            ],
        );
        for (key, value) in [
            ("LUVUS_MODULE_ID", "example.test"),
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
