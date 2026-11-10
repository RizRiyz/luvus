//! Process construction for agent executables started by Luvus workers.
//!
//! Unix keeps `Command::new(program)` and its ordinary `PATH` search. On
//! Windows, Rust's `Command` only appends `.exe` when it searches `PATH`, so an
//! agent installed by npm as `codex.cmd` is reported as "program not found".
//! Workers therefore resolve the program through `PATH` and `PATHEXT` first.
//!
//! A batch file runs through `cmd.exe`, which reparses its arguments and cannot
//! carry the long, multiline, user-controlled task briefing safely. Luvus never
//! hands that briefing to `cmd.exe`: a recognized npm `cmd-shim` is resolved to
//! the interpreter and script it would run (normally `node.exe script.js`), and
//! any other `.bat`/`.cmd` file is refused with an explanatory error.

// The resolver and shim parser are pure so they are tested on every platform,
// but only Windows builds call them.
#![cfg_attr(not(windows), allow(dead_code))]

use std::path::{Path, PathBuf};
use std::process::Command;

/// Largest batch file inspected as a possible npm shim. Real shims are a few
/// hundred bytes.
const MAX_SHIM_BYTES: u64 = 16 * 1024;

/// Build the command for an agent executable.
///
/// Callers add the agent's own arguments afterwards; every argument reaches
/// the agent through the platform's ordinary process-argument rules, never
/// through a shell.
pub fn agent_command(program: &str) -> std::io::Result<Command> {
    #[cfg(windows)]
    {
        windows_agent_command(
            program,
            std::env::var_os("PATH").as_deref(),
            std::env::var_os("PATHEXT").as_deref(),
        )
    }
    #[cfg(not(windows))]
    {
        Ok(Command::new(program))
    }
}

#[cfg(windows)]
fn windows_agent_command(
    program: &str,
    path: Option<&std::ffi::OsStr>,
    pathext: Option<&std::ffi::OsStr>,
) -> std::io::Result<Command> {
    let extensions = launch_extensions(pathext.and_then(|value| value.to_str()));
    let dirs: Vec<PathBuf> = path
        .map(|path| std::env::split_paths(path).collect())
        .unwrap_or_default();
    let resolved = resolve_program(program, &dirs, &extensions, &|path| path.is_file())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "program not found: {program} (searched PATH for {})",
                    extensions.join(", ")
                ),
            )
        })?;
    if !is_batch(&resolved) {
        return Ok(Command::new(resolved));
    }
    let refused = |reason: &str| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{program} resolves to the batch file {}, which {reason}; Luvus does not pass task text through cmd.exe. Install a native executable or an npm package shim",
                resolved.display()
            ),
        )
    };
    let text = read_bounded(&resolved).map_err(|_| refused("could not be read"))?;
    let shim = parse_npm_shim(&text).ok_or_else(|| refused("is not a recognized npm shim"))?;
    let dir = resolved
        .parent()
        .ok_or_else(|| refused("has no parent directory"))?;
    let target = dir.join(&shim.target);
    if !target.is_file() {
        return Err(refused("points to a missing npm package entrypoint"));
    }
    let Some(interpreter) = shim.interpreter else {
        if is_batch(&target) || !has_extension(&target, &[".exe", ".com"]) {
            return Err(refused("does not start a native executable"));
        }
        return Ok(Command::new(target));
    };
    let local = dir.join(&interpreter.local);
    let program = if local.is_file() && has_extension(&local, &[".exe", ".com"]) {
        local
    } else {
        resolve_program(
            &interpreter.fallback,
            &dirs,
            &native_extensions(&extensions),
            &|path| path.is_file(),
        )
        // A fallback written with an explicit `.cmd`/`.bat` suffix must still
        // never run the briefing through cmd.exe.
        .filter(|path| !is_batch(path))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "program not found: {} (needed by the npm shim {})",
                    interpreter.fallback,
                    resolved.display()
                ),
            )
        })?
    };
    let mut command = Command::new(program);
    command.arg(target);
    Ok(command)
}

#[cfg(windows)]
fn read_bounded(path: &Path) -> std::io::Result<String> {
    use std::io::Read;

    let mut text = String::new();
    std::fs::File::open(path)?
        .take(MAX_SHIM_BYTES + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > MAX_SHIM_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "batch file is too large",
        ));
    }
    Ok(text)
}

/// Extensions Luvus may start: native executables and batch files. Script
/// hosts such as `.js` or `.vbs` in `PATHEXT` are never selected for an agent.
const LAUNCHABLE: &[&str] = &[".com", ".exe", ".bat", ".cmd"];

/// The launchable extensions appended to a bare name, in `PATHEXT` order.
fn launch_extensions(pathext: Option<&str>) -> Vec<String> {
    let mut extensions: Vec<String> = pathext
        .unwrap_or(".COM;.EXE;.BAT;.CMD")
        .split(';')
        .map(|extension| extension.trim().to_ascii_lowercase())
        .filter(|extension| LAUNCHABLE.contains(&extension.as_str()))
        .collect();
    extensions.dedup();
    if extensions.is_empty() {
        extensions = LAUNCHABLE.iter().map(|value| value.to_string()).collect();
    }
    extensions
}

/// The native executable extensions among `extensions`, in the same order.
fn native_extensions(extensions: &[String]) -> Vec<String> {
    let native: Vec<String> = extensions
        .iter()
        .filter(|extension| matches!(extension.as_str(), ".com" | ".exe"))
        .cloned()
        .collect();
    if native.is_empty() {
        vec![".com".to_string(), ".exe".to_string()]
    } else {
        native
    }
}

/// Resolve `program` like `cmd.exe` would for an explicit or `PATH` lookup.
/// `extensions` are appended to a name without a launchable extension; a name
/// that already ends in one is used as written, whatever `PATHEXT` says. The
/// current directory is never searched implicitly.
fn resolve_program(
    program: &str,
    dirs: &[PathBuf],
    extensions: &[String],
    is_file: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if program.trim().is_empty() {
        return None;
    }
    let candidates = |base: &Path| -> Vec<PathBuf> {
        if has_extension(base, LAUNCHABLE) {
            vec![base.to_path_buf()]
        } else {
            extensions
                .iter()
                .map(|extension| {
                    let mut name = base.as_os_str().to_os_string();
                    name.push(extension);
                    PathBuf::from(name)
                })
                .collect()
        }
    };
    if program.contains(['/', '\\', ':']) {
        return candidates(Path::new(program))
            .into_iter()
            .find(|candidate| is_file(candidate));
    }
    dirs.iter()
        .filter(|dir| !dir.as_os_str().is_empty())
        .flat_map(|dir| candidates(&dir.join(program)))
        .find(|candidate| is_file(candidate))
}

fn has_extension(path: &Path, extensions: &[impl AsRef<str>]) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            let extension = format!(".{}", extension.to_ascii_lowercase());
            extensions
                .iter()
                .any(|candidate| candidate.as_ref() == extension)
        })
}

fn is_batch(path: &Path) -> bool {
    has_extension(path, &[".bat", ".cmd"])
}

/// What an npm `cmd-shim` batch file runs.
#[derive(Debug, PartialEq, Eq)]
struct NpmShim {
    /// Interpreter that runs `target`, or `None` when `target` is itself the
    /// native executable.
    interpreter: Option<ShimInterpreter>,
    /// Entrypoint relative to the shim's directory.
    target: String,
}

#[derive(Debug, PartialEq, Eq)]
struct ShimInterpreter {
    /// Interpreter beside the shim, relative to its directory (`node.exe`).
    local: String,
    /// Interpreter name searched on `PATH` when `local` is absent (`node`).
    fallback: String,
}

#[derive(Debug, PartialEq, Eq)]
enum ShimLauncher {
    /// `"%_prog%"`, selected by the modern shim's `SET "_prog=..."` lines.
    Prog,
    /// `"%~dp0\node.exe"` in the legacy shim.
    Local(String),
    /// Bare `node` in the legacy shim's fallback branch.
    Path(String),
    /// The target is started directly.
    Direct,
}

/// Recognize the batch files generated by npm's `cmd-shim` (and pnpm's fork).
///
/// The parser is deliberately strict: an extra interpreter argument, another
/// variable expansion, or any unexpected command line makes it return `None`,
/// and the caller then refuses to start the batch file.
fn parse_npm_shim(text: &str) -> Option<NpmShim> {
    let text = text.replace("%~dp0", "%dp0%");
    let lines: Vec<&str> = text
        .lines()
        .map(|line| line.trim().trim_start_matches('@').trim())
        .filter(|line| !line.is_empty())
        .collect();
    // Any line outside the generated templates (for example an extra
    // `SET NODE_OPTIONS=...`) is setup Luvus would silently skip, so refuse.
    if !lines
        .iter()
        .all(|line| line.ends_with("%*") || is_template_line(line))
    {
        return None;
    }
    let launches: Vec<(ShimLauncher, String)> = lines
        .iter()
        .filter(|line| line.ends_with("%*"))
        .map(|line| parse_launch_line(line))
        .collect::<Option<_>>()?;
    match launches.as_slice() {
        [(ShimLauncher::Prog, target)] => {
            let mut local = None;
            let mut fallback = None;
            for line in &lines {
                let Some(value) = line
                    .strip_prefix("SET \"_prog=")
                    .or_else(|| line.strip_prefix("set \"_prog="))
                    .and_then(|rest| rest.strip_suffix('"'))
                else {
                    continue;
                };
                if let Some(relative) = value.strip_prefix("%dp0%\\") {
                    local = Some(shim_relative(relative)?);
                } else {
                    fallback = Some(plain_program_name(value)?);
                }
            }
            Some(NpmShim {
                interpreter: Some(ShimInterpreter {
                    local: local?,
                    fallback: fallback?,
                }),
                target: target.clone(),
            })
        }
        [(ShimLauncher::Local(local), first), (ShimLauncher::Path(fallback), second)]
            if first == second =>
        {
            Some(NpmShim {
                interpreter: Some(ShimInterpreter {
                    local: local.clone(),
                    fallback: fallback.clone(),
                }),
                target: first.clone(),
            })
        }
        [(ShimLauncher::Direct, target)] => Some(NpmShim {
            interpreter: None,
            target: target.clone(),
        }),
        _ => None,
    }
}

/// Whether `line` is one of the non-launch lines `cmd-shim` generates.
fn is_template_line(line: &str) -> bool {
    const FIXED: &[&str] = &[
        "echo off",
        "goto start",
        ":find_dp0",
        "set dp0=%dp0%",
        "exit /b",
        "exit /b %errorlevel%",
        ":start",
        "setlocal",
        "endlocal",
        "call :find_dp0",
        ") else (",
        ")",
        "set pathext=%pathext:;.js;=;%",
    ];
    let lower = line.to_ascii_lowercase();
    if FIXED.contains(&lower.as_str()) {
        return true;
    }
    if let Some(value) = lower
        .strip_prefix("set \"_prog=")
        .and_then(|rest| rest.strip_suffix('"'))
    {
        return match value.strip_prefix("%dp0%\\") {
            Some(relative) => shim_relative(relative).is_some(),
            None => plain_program_name(value).is_some(),
        };
    }
    lower
        .strip_prefix("if exist \"%dp0%\\")
        .and_then(|rest| rest.strip_suffix("\" ("))
        .is_some_and(|relative| shim_relative(relative).is_some())
}

/// Parse one `... "%dp0%\target" %*` line into its launcher and target.
fn parse_launch_line(line: &str) -> Option<(ShimLauncher, String)> {
    let body = line.strip_suffix("%*")?.trim_end();
    let (before, target) = split_last_quoted(body)?;
    let target = shim_relative(target.strip_prefix("%dp0%\\")?)?;
    let before = before.trim_end();
    if let Some(prefix) = before.strip_suffix("\"%_prog%\"") {
        // cmd-shim 5+ restores the caller's environment on the same line;
        // older releases run `"%_prog%"` on a line of its own.
        const ENDLOCAL_PREFIX: &str = "endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% &";
        let prefix = prefix.trim_end();
        return (prefix.is_empty() || prefix.eq_ignore_ascii_case(ENDLOCAL_PREFIX))
            .then_some((ShimLauncher::Prog, target));
    }
    if before.is_empty() {
        return Some((ShimLauncher::Direct, target));
    }
    if let Some((rest, quoted)) = split_last_quoted(before) {
        if !rest.trim().is_empty() {
            return None;
        }
        let local = shim_relative(quoted.strip_prefix("%dp0%\\")?)?;
        return Some((ShimLauncher::Local(local), target));
    }
    Some((ShimLauncher::Path(plain_program_name(before)?), target))
}

/// Split `prefix "quoted"` into `prefix` and the unquoted final token.
fn split_last_quoted(text: &str) -> Option<(&str, &str)> {
    let inner = text.strip_suffix('"')?;
    let start = inner.rfind('"')?;
    Some((&inner[..start], &inner[start + 1..]))
}

/// Accept a shim-relative path with no further expansion or quoting.
fn shim_relative(relative: &str) -> Option<String> {
    let relative = relative.trim_start_matches('\\');
    (!relative.is_empty() && !relative.contains(['%', '"', '!', '^', '&', '|', '<', '>']))
        .then(|| relative.to_string())
}

fn plain_program_name(name: &str) -> Option<String> {
    (!name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-')))
    .then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODERN_SHIM: &str = "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\n\r\nIF EXIST \"%dp0%\\node.exe\" (\r\n  SET \"_prog=%dp0%\\node.exe\"\r\n) ELSE (\r\n  SET \"_prog=node\"\r\n  SET PATHEXT=%PATHEXT:;.JS;=;%\r\n)\r\n\r\nendLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"  \"%dp0%\\node_modules\\@openai\\codex\\bin\\codex.js\" %*\r\n";

    const LEGACY_SHIM: &str = "@IF EXIST \"%~dp0\\node.exe\" (\r\n  \"%~dp0\\node.exe\"  \"%~dp0\\node_modules\\@google\\gemini-cli\\dist\\index.js\" %*\r\n) ELSE (\r\n  @SETLOCAL\r\n  @SET PATHEXT=%PATHEXT:;.JS;=;%\r\n  node  \"%~dp0\\node_modules\\@google\\gemini-cli\\dist\\index.js\" %*\r\n)\r\n";

    const DIRECT_SHIM: &str = "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\n\"%dp0%\\node_modules\\tool\\bin\\tool.exe\"   %*\r\n";

    fn extensions() -> Vec<String> {
        launch_extensions(Some(".COM;.EXE;.BAT;.CMD;.VBS;.JS"))
    }

    #[test]
    fn launch_extensions_follow_pathext_and_ignore_script_hosts() {
        assert_eq!(extensions(), [".com", ".exe", ".bat", ".cmd"]);
        assert_eq!(launch_extensions(Some(".CMD;.Exe;.js")), [".cmd", ".exe"]);
        assert_eq!(launch_extensions(None), [".com", ".exe", ".bat", ".cmd"]);
        assert_eq!(
            launch_extensions(Some(".JS")),
            [".com", ".exe", ".bat", ".cmd"]
        );
    }

    #[test]
    fn resolves_npm_cmd_shims_through_pathext_in_path_order() {
        let first = PathBuf::from(r"C:\Program Files\nodejs");
        let second = PathBuf::from(r"C:\Users\Ada Lovelace\bin");
        let existing = [
            first.join("codex"),
            first.join("codex.cmd"),
            first.join("codex.ps1"),
            second.join("codex.exe"),
        ];
        let is_file = |path: &Path| existing.iter().any(|candidate| candidate == path);
        assert_eq!(
            resolve_program(
                "codex",
                &[first.clone(), second.clone()],
                &extensions(),
                &is_file
            ),
            Some(first.join("codex.cmd"))
        );
        // PATH order wins over PATHEXT order, as in cmd.exe.
        assert_eq!(
            resolve_program(
                "codex",
                &[second.clone(), first.clone()],
                &extensions(),
                &is_file
            ),
            Some(second.join("codex.exe"))
        );
        // The extensionless POSIX shim beside codex.cmd is never selected.
        assert_eq!(
            resolve_program(
                "codex",
                std::slice::from_ref(&first),
                &[".exe".to_string()],
                &is_file
            ),
            None
        );
        assert_eq!(
            resolve_program(
                "codex.cmd",
                &[second.clone(), first.clone()],
                &extensions(),
                &is_file
            ),
            Some(first.join("codex.cmd"))
        );
    }

    #[test]
    fn resolves_explicit_paths_with_spaces_without_searching_path() {
        let explicit = r"C:\Program Files\nodejs\codex";
        let existing = [PathBuf::from(r"C:\Program Files\nodejs\codex.cmd")];
        let is_file = |path: &Path| existing.iter().any(|candidate| candidate == path);
        let dirs = [PathBuf::from(r"C:\elsewhere")];
        assert_eq!(
            resolve_program(explicit, &dirs, &extensions(), &is_file),
            Some(existing[0].clone())
        );
        assert_eq!(
            resolve_program(&format!("{explicit}.cmd"), &dirs, &extensions(), &is_file),
            Some(existing[0].clone())
        );
        assert_eq!(
            resolve_program(
                r"C:\Program Files\nodejs\gemini",
                &dirs,
                &extensions(),
                &is_file
            ),
            None
        );
    }

    #[test]
    fn explicit_launchable_suffixes_do_not_depend_on_pathext() {
        let only_cmd = launch_extensions(Some(".CMD"));
        assert_eq!(only_cmd, [".cmd"]);
        // Build both spellings so the test also runs where `\` is not a path
        // separator; on Windows they are the same path.
        let explicit = PathBuf::from(r"C:\tools\agent.exe");
        let tools = PathBuf::from(r"C:\tools");
        let existing = [explicit.clone(), tools.join("agent.exe")];
        let is_file = |path: &Path| existing.iter().any(|candidate| candidate == path);
        assert_eq!(
            resolve_program(r"C:\tools\agent.exe", &[], &only_cmd, &is_file),
            Some(explicit)
        );
        assert_eq!(
            resolve_program(
                "agent.exe",
                std::slice::from_ref(&tools),
                &only_cmd,
                &is_file
            ),
            Some(existing[1].clone())
        );
        // PATHEXT still decides what is appended to a bare name.
        assert_eq!(
            resolve_program("agent", std::slice::from_ref(&tools), &only_cmd, &is_file),
            None
        );
    }

    #[test]
    fn interpreter_fallback_searches_native_executables_in_pathext_order() {
        assert_eq!(native_extensions(&extensions()), [".com", ".exe"]);
        let ordered = launch_extensions(Some(".CMD;.EXE;.BAT;.COM"));
        assert_eq!(native_extensions(&ordered), [".exe", ".com"]);
        assert_eq!(
            native_extensions(&launch_extensions(Some(".CMD"))),
            [".com", ".exe"]
        );
        // A version-manager node.cmd earlier on PATH does not hide node.exe.
        let shims = PathBuf::from(r"C:\Users\Ada\shims");
        let nodejs = PathBuf::from(r"C:\Program Files\nodejs");
        let existing = [shims.join("node.cmd"), nodejs.join("node.exe")];
        let is_file = |path: &Path| existing.iter().any(|candidate| candidate == path);
        assert_eq!(
            resolve_program(
                "node",
                &[shims, nodejs.clone()],
                &native_extensions(&extensions()),
                &is_file
            ),
            Some(nodejs.join("node.exe"))
        );
    }

    #[test]
    fn missing_or_empty_programs_do_not_resolve() {
        let dirs = [PathBuf::from(r"C:\bin"), PathBuf::new()];
        let never = |_: &Path| false;
        assert_eq!(resolve_program("codex", &dirs, &extensions(), &never), None);
        assert_eq!(resolve_program("  ", &dirs, &extensions(), &|_| true), None);
        // An empty PATH entry is not the current directory.
        let only_cwd = [PathBuf::new()];
        assert_eq!(
            resolve_program("codex", &only_cwd, &extensions(), &|_| true),
            None
        );
    }

    #[test]
    fn parses_the_modern_npm_cmd_shim() {
        assert_eq!(
            parse_npm_shim(MODERN_SHIM),
            Some(NpmShim {
                interpreter: Some(ShimInterpreter {
                    local: "node.exe".into(),
                    fallback: "node".into(),
                }),
                target: r"node_modules\@openai\codex\bin\codex.js".into(),
            })
        );
        assert!(parse_npm_shim(&MODERN_SHIM.replace("\r\n", "\n")).is_some());
    }

    #[test]
    fn parses_the_legacy_and_direct_npm_cmd_shims() {
        assert_eq!(
            parse_npm_shim(LEGACY_SHIM),
            Some(NpmShim {
                interpreter: Some(ShimInterpreter {
                    local: "node.exe".into(),
                    fallback: "node".into(),
                }),
                target: r"node_modules\@google\gemini-cli\dist\index.js".into(),
            })
        );
        assert_eq!(
            parse_npm_shim(DIRECT_SHIM),
            Some(NpmShim {
                interpreter: None,
                target: r"node_modules\tool\bin\tool.exe".into(),
            })
        );
    }

    #[test]
    fn rejects_batch_files_that_are_not_strict_npm_shims() {
        for text in [
            "@echo off\r\ncodex.exe %*\r\n",
            "@echo off\r\necho hi\r\n",
            &MODERN_SHIM.replace("\"%_prog%\"  ", "\"%_prog%\" --inspect "),
            &MODERN_SHIM.replace(r"bin\codex.js", r"bin\%TARGET%.js"),
            &MODERN_SHIM.replace("SET \"_prog=node\"", "SET \"_prog=node & calc\""),
            &MODERN_SHIM.replace("SET \"_prog=node\"\r\n", ""),
            &MODERN_SHIM.replace("& \"%_prog%\"", "& calc & \"%_prog%\""),
            &LEGACY_SHIM.replacen("dist\\index.js", "dist\\other.js", 1),
            &format!("{MODERN_SHIM}\"%dp0%\\second.exe\" %*\r\n"),
            // Extra setup beside an otherwise valid launch line would be
            // skipped when Luvus starts the interpreter directly.
            &MODERN_SHIM.replace(
                "CALL :find_dp0\r\n",
                "CALL :find_dp0\r\nSET NODE_OPTIONS=--require=hook.js\r\n",
            ),
            &LEGACY_SHIM.replace(
                "@SETLOCAL\r\n",
                "@SETLOCAL\r\n@SET \"NODE_PATH=C:\\pnpm\\node_modules\"\r\n",
            ),
            &DIRECT_SHIM.replace("SETLOCAL\r\n", "SETLOCAL\r\ncd /d %dp0%\r\n"),
        ] {
            assert_eq!(parse_npm_shim(text), None, "{text}");
        }
    }

    #[test]
    fn parses_the_cmd_shim_4_layout_with_its_own_launch_line() {
        let shim = "@ECHO off\r\nSETLOCAL\r\nCALL :find_dp0\r\n\r\nIF EXIST \"%dp0%\\node.exe\" (\r\n  SET \"_prog=%dp0%\\node.exe\"\r\n) ELSE (\r\n  SET \"_prog=node\"\r\n  SET PATHEXT=%PATHEXT:;.JS;=;%\r\n)\r\n\r\n\"%_prog%\"  \"%dp0%\\node_modules\\tool\\cli.js\" %*\r\nENDLOCAL\r\nEXIT /b %errorlevel%\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n";
        assert_eq!(
            parse_npm_shim(shim),
            Some(NpmShim {
                interpreter: Some(ShimInterpreter {
                    local: "node.exe".into(),
                    fallback: "node".into(),
                }),
                target: r"node_modules\tool\cli.js".into(),
            })
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_agent_command_is_unchanged() {
        let command = agent_command("codex").unwrap();
        assert_eq!(command.get_program(), "codex");
        assert_eq!(command.get_args().count(), 0);
    }

    #[cfg(windows)]
    mod windows {
        use super::super::*;
        use super::MODERN_SHIM;

        struct TempDir(PathBuf);

        impl TempDir {
            fn new(label: &str) -> Self {
                let path = std::env::temp_dir().join(format!(
                    "luvus agent command {label} {}-{}",
                    std::process::id(),
                    crate::ids::public_id("test")
                ));
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        fn install_modern_shim(dir: &Path, name: &str) -> PathBuf {
            let script = dir.join(r"node_modules\@openai\codex\bin\codex.js");
            std::fs::create_dir_all(script.parent().unwrap()).unwrap();
            std::fs::write(
                &script,
                "require('fs').writeFileSync(process.env.LUVUS_SHIM_TEST_OUT, JSON.stringify(process.argv.slice(2)));\n",
            )
            .unwrap();
            std::fs::write(dir.join(format!("{name}.cmd")), MODERN_SHIM).unwrap();
            script
        }

        fn build(program: &str, path: &[&Path]) -> std::io::Result<Command> {
            let path = std::env::join_paths(path).unwrap();
            windows_agent_command(
                program,
                Some(&path),
                Some(std::ffi::OsStr::new(".COM;.EXE;.BAT;.CMD")),
            )
        }

        fn node_on_path() -> Option<PathBuf> {
            let path = std::env::var_os("PATH")?;
            let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
            resolve_program("node", &dirs, &[".exe".to_string()], &|path| path.is_file())
        }

        #[test]
        fn npm_shim_runs_its_script_through_the_interpreter_not_cmd() {
            let shim_dir = TempDir::new("shim");
            let node_dir = TempDir::new("node");
            let script = install_modern_shim(&shim_dir.0, "codex");
            std::fs::write(node_dir.0.join("node.exe"), b"").unwrap();

            let command = build("codex", &[&shim_dir.0, &node_dir.0]).unwrap();
            assert_eq!(
                command.get_program(),
                node_dir.0.join("node.exe").as_os_str()
            );
            let args: Vec<_> = command.get_args().collect();
            assert_eq!(args, [script.as_os_str()]);

            // A node.exe beside the shim wins, exactly as the shim prefers it.
            std::fs::write(shim_dir.0.join("node.exe"), b"").unwrap();
            let command = build("codex", &[&shim_dir.0, &node_dir.0]).unwrap();
            assert_eq!(
                command.get_program(),
                shim_dir.0.join("node.exe").as_os_str()
            );
        }

        #[test]
        fn missing_program_and_interpreter_report_clear_errors() {
            let empty = TempDir::new("empty");
            let error = build("codex", &[&empty.0]).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
            assert!(
                error.to_string().contains("program not found: codex"),
                "{error}"
            );
            assert!(error.to_string().contains(".cmd"), "{error}");

            install_modern_shim(&empty.0, "codex");
            let error = build("codex", &[&empty.0]).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
            assert!(
                error.to_string().contains("program not found: node"),
                "{error}"
            );
        }

        #[test]
        fn a_batch_interpreter_fallback_is_never_started() {
            let dir = TempDir::new("batch fallback");
            install_modern_shim(&dir.0, "codex");
            let shim = MODERN_SHIM.replace("SET \"_prog=node\"", "SET \"_prog=node.cmd\"");
            std::fs::write(dir.0.join("codex.cmd"), shim).unwrap();
            std::fs::write(dir.0.join("node.cmd"), "@echo off\r\n").unwrap();
            let error = build("codex", &[&dir.0]).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
            assert!(
                error.to_string().contains("program not found: node.cmd"),
                "{error}"
            );
        }

        #[test]
        fn arbitrary_batch_files_are_refused_instead_of_run_through_cmd() {
            let dir = TempDir::new("batch");
            std::fs::write(
                dir.0.join("agent.cmd"),
                "@echo off\r\nreal-agent.exe %*\r\n",
            )
            .unwrap();
            let error = build("agent", &[&dir.0]).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            assert!(
                error.to_string().contains("not a recognized npm shim"),
                "{error}"
            );

            std::fs::write(
                dir.0.join("huge.cmd"),
                format!("{}{MODERN_SHIM}", " ".repeat(MAX_SHIM_BYTES as usize)),
            )
            .unwrap();
            assert!(build("huge", &[&dir.0]).is_err());

            // A shim that points at a missing package is refused, not run.
            std::fs::write(dir.0.join("gone.cmd"), MODERN_SHIM).unwrap();
            let error = build("gone", &[&dir.0]).unwrap_err();
            assert!(error.to_string().contains("missing npm package"), "{error}");
        }

        #[test]
        fn native_executables_are_started_by_full_path() {
            let dir = TempDir::new("native");
            std::fs::write(dir.0.join("claude.exe"), b"").unwrap();
            std::fs::write(dir.0.join("claude.cmd"), b"@echo off\r\n").unwrap();
            let command = build("claude", &[&dir.0]).unwrap();
            assert_eq!(command.get_program(), dir.0.join("claude.exe").as_os_str());
        }

        #[test]
        fn hostile_multiline_briefing_reaches_the_shimmed_agent_verbatim() {
            let Some(node) = node_on_path() else {
                eprintln!("skipping: node.exe is not on PATH");
                return;
            };
            let shim_dir = TempDir::new("live shim");
            install_modern_shim(&shim_dir.0, "codex");
            let output = shim_dir.0.join("argv.json");
            let briefing = format!(
                "first line\r\nsecond \"quoted\" & calc.exe | more > nul ^ %PATH% !PATH! %*\n{}\n\\\"trailing\\",
                "x".repeat(8 * 1024)
            );
            let args = ["exec", "--sandbox", "read-only", "two words", ""];

            let mut command = build("codex", &[&shim_dir.0, node.parent().unwrap()]).unwrap();
            let status = command
                .args(args)
                .arg(&briefing)
                .env("LUVUS_SHIM_TEST_OUT", &output)
                .status()
                .unwrap();
            assert!(status.success());
            let received: Vec<String> =
                serde_json::from_str(&std::fs::read_to_string(&output).unwrap()).unwrap();
            let mut expected: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
            expected.push(briefing);
            assert_eq!(received, expected);
        }
    }
}
