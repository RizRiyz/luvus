//! OS-specific bits, isolated here so core modules stay portable (docs/03 §7).

use std::path::{Path, PathBuf};

#[cfg(windows)]
mod windows;

/// Do two paths name the same folder? (docs/43 WIN-6.)
///
/// Node lookup used to compare `PathBuf`s with `==`, so any difference in
/// *spelling* read as "not open" and luvus added a duplicate node instead of
/// focusing the existing one. Windows has many spellings for one path — case
/// (`C:\Proj` vs `c:\proj`, which the filesystem treats as equal), the `\\?\`
/// verbatim prefix that `canonicalize` returns, `/` accepted in place of `\`,
/// and trailing separators — and every one of them defeated `==`.
///
/// Deliberately **lexical only, no IO**: this runs on user actions that can
/// repeat, and a `canonicalize` per candidate would put syscalls on that path
/// for no gain (the client always sends `std::env::current_dir()`, which is
/// already resolved). Consequence: two *different* spellings that only a
/// symlink resolve would unify (`/tmp` vs `/private/tmp` on macOS) still
/// compare unequal. That is the intended trade.
pub fn same_path(a: &Path, b: &Path) -> bool {
    path_key(a) == path_key(b)
}

/// True when `child` is `parent` or a folder inside it (docs/43 WIN-6 spelling).
pub fn is_subpath(child: &Path, parent: &Path) -> bool {
    let child = path_key(child);
    let parent = path_key(parent);
    child == parent
        || child.starts_with(&format!("{parent}\\"))
        || child.starts_with(&format!("{parent}/"))
}

/// The comparison key for [`same_path`] — normalized spelling, never displayed.
/// The node keeps the user's original spelling for its label and pane cwd.
fn path_key(p: &Path) -> String {
    let s = p.to_string_lossy();
    // `\\?\C:\proj` and `C:\proj` are the same folder.
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    #[cfg(windows)]
    // Windows accepts `/` as a separator and is case-insensitive.
    let s = s.replace('/', "\\").to_lowercase();
    // Drop trailing separators so `proj\` == `proj`, but never eat a bare root
    // (`/` or `C:\`), which would make every root compare equal to the empty path.
    let sep: &[char] = &['/', '\\'];
    let trimmed = s.trim_end_matches(sep);
    if trimmed.is_empty() || trimmed.ends_with(':') {
        return s.to_string();
    }
    trimmed.to_string()
}

/// Keep a spawned console program from flashing a window on Windows.
///
/// `luvus server` runs detached (`main::spawn_server` uses `DETACHED_PROCESS`),
/// so it has no console of its own. Windows then hands every console child it
/// spawns a fresh `conhost.exe` **with a visible window** — and the git poller
/// alone spawns one every ~2 s per workspace, which strobed black windows over
/// the desktop ~45 times a minute. `CREATE_NO_WINDOW` (0x0800_0000) gives the
/// child a console without a window; inherited/piped stdio handles are
/// unaffected, so captured output still arrives.
///
/// Only for spawns luvus captures or discards (`.output()`, `.status()`,
/// null stdio). **Never** put this on the PTY/pane child or an agent the user
/// interacts with — those need their real console.
pub fn no_window(cmd: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// The user's home directory, cross-platform (`$HOME`, else `%USERPROFILE%`).
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Resolve a configured shell `choice` to a concrete command to spawn.
///
/// `LUVUS_SHELL` always wins (the explicit escape hatch — set it in your shell
/// profile). Otherwise the choice (from Settings → Pane Layout → Shell):
/// `""`/`"default"` picks the platform default; `"powershell"` and `"cmd"` are
/// Windows shells; anything else is treated as a literal command. The platform
/// default is the login `SHELL` on Unix and **PowerShell** on Windows
/// (`pwsh.exe`, then `powershell.exe`), since `COMSPEC` is always `cmd.exe`
/// regardless of the shell you launched from and so can't reveal PowerShell.
pub fn resolve_shell(choice: &str) -> String {
    if let Some(s) = std::env::var_os("LUVUS_SHELL") {
        if !s.is_empty() {
            return s.to_string_lossy().into_owned();
        }
    }
    match choice {
        "" | "default" => platform_default_shell(),
        "powershell" => find_on_path("pwsh.exe")
            .or_else(|| find_on_path("pwsh"))
            .or_else(|| find_on_path("powershell.exe"))
            .unwrap_or_else(platform_default_shell),
        "cmd" => std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string()),
        other => other.to_string(),
    }
}

#[cfg(windows)]
fn platform_default_shell() -> String {
    find_on_path("pwsh.exe")
        .or_else(|| find_on_path("powershell.exe"))
        .unwrap_or_else(|| std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string()))
}

#[cfg(not(windows))]
fn platform_default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

/// Argv that runs `cmd` inside `shell` and then keeps that shell open.
///
/// POSIX shells deliberately return `None`: callers spawn the user's normal
/// interactive shell and queue `cmd` through its PTY, after `.zshrc`, `.bashrc`,
/// fish configuration, NVM, mise, and similar environment setup has run.
/// PowerShell loads its profile for `-NoExit -Command`, so it can still start
/// directly without exposing a prompt first.
pub fn shell_run_then_interactive(shell: &str, cmd: &str) -> Option<Vec<String>> {
    if shell.contains('\'') {
        return None; // a quote in the shell path would break the exec quoting
    }
    let base = std::path::Path::new(shell)
        .file_name()?
        .to_str()?
        .to_ascii_lowercase();
    match base.strip_suffix(".exe").unwrap_or(&base) {
        "sh" | "bash" | "zsh" | "dash" | "ksh" | "fish" => None,
        "pwsh" | "powershell" => Some(vec![
            shell.to_string(),
            "-NoExit".to_string(),
            "-Command".to_string(),
            cmd.to_string(),
        ]),
        // cmd.exe can't take the single-quoted id literally — let the caller
        // fall back to typing the command.
        _ => None,
    }
}

/// Resolve an executable name to its full path by scanning `PATH`.
fn find_on_path(exe: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(exe))
        .find(|full| full.is_file())
        .map(|full| full.to_string_lossy().into_owned())
}

/// Is a terminal editor `exe` on `PATH`? (On Windows, also try `exe.exe`.)
fn editor_on_path(exe: &str) -> bool {
    find_on_path(exe).is_some() || (cfg!(windows) && find_on_path(&format!("{exe}.exe")).is_some())
}

/// Terminal editors luvus can offer to open a file with (docs/38): the ones
/// actually installed on `PATH`, in preference order, plus `$EDITOR` when set
/// and not already covered. Each entry is `(run command, display label)` — the
/// command is spawned as a real pane, the label is what Settings/the menu shows.
///
/// Computed once at startup and cached on `App` (a handful of `PATH` stats), so
/// it never runs on the render path. A dead option can only appear if an editor
/// is uninstalled mid-session, and the open path degrades gracefully then.
pub fn editor_choices() -> Vec<(String, String)> {
    // (probe name, run command, label). `emacs -nw` forces the terminal UI.
    const KNOWN: &[(&str, &str, &str)] = &[
        ("vim", "vim", "vim"),
        ("nvim", "nvim", "nvim"),
        ("nano", "nano", "nano"),
        ("vi", "vi", "vi"),
        ("hx", "hx", "helix"),
        ("micro", "micro", "micro"),
        ("emacs", "emacs -nw", "emacs"),
    ];
    let mut out: Vec<(String, String)> = Vec::new();
    for (exe, cmd, label) in KNOWN {
        if editor_on_path(exe) {
            out.push(((*cmd).to_string(), (*label).to_string()));
        }
    }
    // $EDITOR, honored verbatim (so `EDITOR="emacs -nw"` works) unless its base
    // name is already listed above.
    if let Ok(ed) = std::env::var("EDITOR") {
        let ed = ed.trim();
        let first = ed.split_whitespace().next().unwrap_or("");
        let base = std::path::Path::new(first)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(first);
        let already = !base.is_empty()
            && (KNOWN.iter().any(|(exe, _, _)| *exe == base)
                || out
                    .iter()
                    .any(|(c, _)| c.split_whitespace().next() == Some(base)));
        if !ed.is_empty() && !already {
            out.push((ed.to_string(), format!("$EDITOR ({base})")));
        }
    }
    out
}

/// Shell choices offered in Settings, as `(keyword, display label)`. The choice
/// is **Windows-only** — elsewhere panes always use the login `$SHELL`, so there
/// is nothing to pick. The keyword is stored in config and passed to
/// [`resolve_shell`].
#[cfg(windows)]
pub fn shell_choices() -> &'static [(&'static str, &'static str)] {
    &[
        ("default", "Default"),
        ("powershell", "PowerShell"),
        ("cmd", "Command Prompt"),
    ]
}

/// Display label for a stored shell keyword (falls back to the keyword itself).
#[cfg(windows)]
pub fn shell_label(choice: &str) -> &str {
    shell_choices()
        .iter()
        .find(|(k, _)| *k == choice)
        .map(|(_, label)| *label)
        .unwrap_or(choice)
}

/// The current working directory of a process, or `None` if unavailable.
/// Used to make a workspace follow where the user actually works.
#[cfg(target_os = "macos")]
pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    use std::mem;
    unsafe {
        let mut info: libc::proc_vnodepathinfo = mem::zeroed();
        let size = mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        let n = libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        );
        if n < size {
            return None;
        }
        // `vip_path` is MAXPATHLEN (1024) bytes of a null-terminated path.
        let raw = std::slice::from_raw_parts(
            info.pvi_cdir.vip_path.as_ptr() as *const u8,
            mem::size_of_val(&info.pvi_cdir.vip_path),
        );
        let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        if end == 0 {
            return None;
        }
        Some(PathBuf::from(
            String::from_utf8_lossy(&raw[..end]).into_owned(),
        ))
    }
}

#[cfg(target_os = "linux")]
pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(windows)]
pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    windows::process_cwd(pid)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub fn process_cwd(_pid: u32) -> Option<PathBuf> {
    None
}

/// PID-reuse-safe process lifetime marker captured for the public terminal
/// backend. The value is opaque on the wire and compared only on its native OS.
#[cfg(target_os = "linux")]
pub fn process_start_marker(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is parenthesized and may itself contain spaces or `)`;
    // the final `)` is followed by field 3 (state). Field 22 (starttime) is the
    // 20th token in that suffix, indexed from zero as 19.
    let tail = stat.rsplit_once(") ")?.1;
    tail.split_whitespace().nth(19).map(str::to_string)
}

#[cfg(target_os = "macos")]
pub fn process_start_marker(pid: u32) -> Option<String> {
    use std::mem::{size_of, zeroed};
    unsafe {
        let mut info: libc::proc_bsdinfo = zeroed();
        let size = size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let read = libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        );
        if read < size {
            return None;
        }
        Some(format!(
            "{}.{:06}",
            info.pbi_start_tvsec, info.pbi_start_tvusec
        ))
    }
}

#[cfg(windows)]
pub fn process_start_marker(pid: u32) -> Option<String> {
    windows::process_start_marker(pid)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub fn process_start_marker(_pid: u32) -> Option<String> {
    None
}

#[cfg(windows)]
pub fn process_belongs_to_current_user(pid: u32) -> bool {
    windows::process_belongs_to_current_user(pid)
}

#[cfg(target_os = "macos")]
fn process_executable(pid: u32) -> Option<PathBuf> {
    let mut buffer = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: `buffer` is writable for the size passed to `proc_pidpath`, and
    // the returned byte count is checked before constructing the path.
    let len = unsafe {
        libc::proc_pidpath(
            pid as libc::c_int,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
        )
    };
    if len <= 0 {
        return None;
    }
    buffer.truncate(len as usize);
    Some(PathBuf::from(String::from_utf8_lossy(&buffer).into_owned()))
}

/// True when `pid` is another Luvus process owned by this account.
/// `server stop` uses this before force-killing an unresponsive server.
pub fn is_stoppable_luvus_pid(pid: u32) -> bool {
    if pid == 0 || pid == std::process::id() {
        return false;
    }
    #[cfg(windows)]
    {
        if !process_belongs_to_current_user(pid) {
            return false;
        }
        windows::process_executable(pid).is_some_and(|executable| {
            let name = executable.rsplit(['\\', '/']).next().unwrap_or(&executable);
            name.eq_ignore_ascii_case("luvus.exe")
        })
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .is_ok_and(|comm| comm.trim() == "luvus")
    }
    #[cfg(target_os = "macos")]
    {
        process_executable(pid).is_some_and(|executable| {
            executable
                .file_name()
                .is_some_and(|name| name == std::ffi::OsStr::new("luvus"))
        })
    }
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    {
        let Some(info) = process_tree(pid).into_iter().next() else {
            return false;
        };
        let name = info
            .command
            .split_whitespace()
            .next()
            .unwrap_or(&info.command);
        let base = name.rsplit('/').next().unwrap_or(name);
        base == "luvus"
    }
    #[cfg(not(any(windows, unix)))]
    {
        false
    }
}

/// End `pid` and its children. Used only after [`is_stoppable_luvus_pid`].
pub fn force_terminate(pid: u32) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        let status = no_window(
            std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null()),
        )
        .status()?;
        if status.success() || !is_stoppable_luvus_pid(pid) {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "taskkill exited with {status}"
            )))
        }
    }
    #[cfg(unix)]
    {
        let pid_t = pid as libc::pid_t;
        let mut tree = process_tree(pid);
        if tree.is_empty() {
            tree.push(ProcInfo {
                pid,
                depth: 0,
                command: String::new(),
            });
        }
        let pgid = unsafe { libc::getpgid(pid_t) };
        if pgid == pid_t {
            let _ = unsafe { libc::kill(-pid_t, libc::SIGKILL) };
        }
        let mut root_error = None;
        for proc in tree.iter().rev() {
            let result = unsafe { libc::kill(proc.pid as libc::pid_t, libc::SIGKILL) };
            if result != 0 && proc.pid == pid {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    root_error = Some(error);
                }
            }
        }
        match root_error {
            Some(error) if is_stoppable_luvus_pid(pid) => Err(error),
            _ => Ok(()),
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = pid;
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
    }
}

/// One process running under a pane, for the "what is actually running?" overlay.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcInfo {
    pub pid: u32,
    /// Nesting under the pane's own shell (0 = the shell itself).
    pub depth: u16,
    /// The full command line, exactly as the OS has it — never truncated.
    pub command: String,
}

/// The process table: `pid → command`, and `ppid → children` for walking it.
///
/// Gated with its only consumer, `ps_table` — process discovery shells out to
/// `ps`, so on Windows neither exists and an ungated alias was dead code there
/// (the one warning the Windows cross-check emitted).
#[cfg(unix)]
type PsTable = (
    std::collections::HashMap<u32, String>,
    std::collections::HashMap<u32, Vec<u32>>,
);

/// The whole process table: `pid → command` plus `ppid → children`.
/// `None` when the platform cannot tell, which callers must distinguish from an
/// empty table: "I cannot tell" is not "nothing is running".
///
/// On **Linux (including WSL)** this reads `/proc` directly rather than shelling
/// out to `ps`. `/proc` is ground truth the `ps` binary merely formats, and the
/// direct read fixes the setups where the `ps` path silently returns nothing —
/// a **busybox `ps`** on a musl/Alpine WSL distro (no `ppid` column, so
/// `-Ao ppid=` yields garbage), a minimal image with no procps, or a stripped
/// `PATH` in the detached server. Every one of those demoted agent detection to
/// title/screen-text only, which made agents that don't print their own name
/// (opencode) vanish from the sidebar. It also skips a subprocess spawn on a
/// periodic path. macOS/BSD have no comparable `/proc`, so they use `ps`.
#[cfg(unix)]
fn ps_table() -> Option<PsTable> {
    #[cfg(target_os = "linux")]
    if let Some(t) = proc_fs_table() {
        return Some(t);
    }
    ps_command_table()
}

/// Walk `/proc/<pid>/{stat,cmdline}` into the process table. `None` only if
/// `/proc` itself can't be listed (not mounted), so callers fall back to `ps`.
#[cfg(target_os = "linux")]
fn proc_fs_table() -> Option<PsTable> {
    use std::collections::HashMap;
    let mut cmd: HashMap<u32, String> = HashMap::new();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        // `/proc/<pid>/stat` is `pid (comm) state ppid …`; comm can contain
        // spaces and parens, so split after the *last* ')' before reading the
        // fixed fields. ppid is then the second whitespace token (after state).
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some((_, tail)) = stat.rsplit_once(')') else {
            continue;
        };
        let mut fields = tail.split_whitespace();
        let _state = fields.next();
        let Some(Ok(ppid)) = fields.next().map(str::parse::<u32>) else {
            continue;
        };
        // argv from `cmdline` (NUL-separated), space-joined to match `ps args`.
        // An empty cmdline (kernel thread / zombie) falls back to the bracketed
        // comm — never an agent, but keeps the tree complete.
        let command = match std::fs::read(format!("/proc/{pid}/cmdline")) {
            Ok(bytes) if bytes.iter().any(|&b| b != 0) => bytes
                .split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(String::from_utf8_lossy)
                .collect::<Vec<_>>()
                .join(" "),
            _ => stat
                .split_once('(')
                .and_then(|(_, r)| r.rsplit_once(')'))
                .map(|(c, _)| format!("[{c}]"))
                .unwrap_or_default(),
        };
        if command.is_empty() {
            continue;
        }
        cmd.insert(pid, command);
        children.entry(ppid).or_default().push(pid);
    }
    // `/proc` always lists at least this process on Linux; an empty map means
    // the read_dir yielded nothing usable, so let `ps` have a try.
    (!cmd.is_empty()).then_some((cmd, children))
}

/// The process table from one `ps` invocation — the portable fallback and the
/// path macOS/BSD always take. See [`ps_table`] for why Linux prefers `/proc`.
#[cfg(unix)]
fn ps_command_table() -> Option<PsTable> {
    use std::collections::HashMap;
    let out = match std::process::Command::new("ps")
        .args(["-Ao", "pid=,ppid=,args="])
        .output()
    {
        Ok(o) if o.status.success() => o.stdout,
        _ => return None,
    };
    let text = String::from_utf8_lossy(&out);
    let mut cmd: HashMap<u32, String> = HashMap::new();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(ppid)) = (it.next(), it.next()) else {
            continue;
        };
        let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
            continue;
        };
        // Everything after the two numeric columns is the command, spaces intact.
        let rest = line
            .splitn(3, |c: char| c.is_whitespace())
            .nth(2)
            .unwrap_or("")
            .trim_start();
        if rest.is_empty() {
            continue;
        }
        cmd.insert(pid, rest.to_string());
        children.entry(ppid).or_default().push(pid);
    }
    Some((cmd, children))
}

type ProcessTrees = std::collections::HashMap<u32, Vec<(u32, u16)>>;
type ProcessCommands = std::collections::HashMap<u32, Vec<String>>;

/// Capture one bounded platform process table and project every requested pane
/// root into descendant pid trees and, when requested, command lines. Keeping
/// both projections behind this boundary lets periodic CWD and agent scans
/// share the expensive OS snapshot without coupling their app-level cadence.
#[cfg(unix)]
fn pane_process_snapshot(
    roots: &[u32],
    include_trees: bool,
    include_commands: bool,
) -> (ProcessTrees, Option<ProcessCommands>) {
    use std::collections::{HashMap, HashSet};
    let Some((commands_by_pid, children)) = ps_table() else {
        return (HashMap::new(), None);
    };
    let trees: ProcessTrees = if include_trees {
        roots
            .iter()
            .copied()
            .map(|root| {
                let mut nodes = Vec::new();
                let mut seen = HashSet::new();
                let mut stack = vec![(root, 0_u16)];
                while let Some((pid, depth)) = stack.pop() {
                    if !seen.insert(pid) || nodes.len() >= 64 {
                        continue;
                    }
                    nodes.push((pid, depth));
                    if let Some(kids) = children.get(&pid) {
                        for &child in kids.iter().rev() {
                            stack.push((child, depth.saturating_add(1)));
                        }
                    }
                }
                (root, nodes)
            })
            .collect()
    } else {
        HashMap::new()
    };
    let commands = include_commands.then(|| {
        roots
            .iter()
            .copied()
            .map(|root| {
                // Preserve the command projection's established traversal
                // order independently of the depth-first CWD projection.
                let mut found = Vec::new();
                let mut seen = HashSet::new();
                let mut stack = vec![root];
                while let Some(pid) = stack.pop() {
                    if !seen.insert(pid) || found.len() >= 64 {
                        continue;
                    }
                    if let Some(command) = commands_by_pid.get(&pid) {
                        found.push(command.clone());
                    }
                    if let Some(kids) = children.get(&pid) {
                        stack.extend(kids.iter().copied());
                    }
                }
                (root, found)
            })
            .collect()
    });
    (trees, commands)
}

#[cfg(windows)]
fn pane_process_snapshot(
    roots: &[u32],
    include_trees: bool,
    include_commands: bool,
) -> (ProcessTrees, Option<ProcessCommands>) {
    windows::pane_process_snapshot(roots, include_trees, include_commands)
        .unwrap_or_else(|| (ProcessTrees::new(), None))
}

#[cfg(not(any(unix, windows)))]
fn pane_process_snapshot(
    _roots: &[u32],
    _include_trees: bool,
    _include_commands: bool,
) -> (ProcessTrees, Option<ProcessCommands>) {
    (ProcessTrees::new(), None)
}

/// Process identities running under each of `roots` (the root's own included),
/// from one platform snapshot. This batched form lets agent detection cover
/// every pane without one process-table operation per pane. `None` means the
/// platform cannot tell.
pub fn descendant_commands(roots: &[u32]) -> Option<ProcessCommands> {
    pane_process_snapshot(roots, false, true).1
}

/// Every process running under `root` (inclusive), depth-first, newest branch
/// last. This is the honest answer to "what command is this agent running?":
/// an agent's own UI usually *elides* long commands (`Bash(cargo test …)`), and
/// those characters never reach luvus, so the screen simply cannot be expanded.
/// The OS still knows the real argv, and luvus owns the pane's child pid.
///
/// **Call on demand only** (opening the overlay), never per frame: it captures
/// one bounded platform process snapshot and walks the result. Empty on
/// unsupported platforms, and on any failure — the caller degrades to showing
/// just the pane's own command.
#[cfg(unix)]
pub fn process_tree(root: u32) -> Vec<ProcInfo> {
    let Some((cmd, children)) = ps_table() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    // Iterative DFS so a pathological tree can't blow the stack; the visited set
    // makes a cyclic/reparented table (pid reuse) terminate.
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![(root, 0u16)];
    while let Some((pid, depth)) = stack.pop() {
        if !seen.insert(pid) || out.len() >= 64 {
            continue;
        }
        if let Some(c) = cmd.get(&pid) {
            out.push(ProcInfo {
                pid,
                depth,
                command: c.clone(),
            });
        }
        if let Some(kids) = children.get(&pid) {
            for &k in kids.iter().rev() {
                stack.push((k, depth.saturating_add(1)));
            }
        }
    }
    out
}

#[cfg(windows)]
pub fn process_tree(root: u32) -> Vec<ProcInfo> {
    windows::process_tree(root)
}

#[cfg(not(any(unix, windows)))]
pub fn process_tree(_root: u32) -> Vec<ProcInfo> {
    Vec::new()
}

/// One pane's live cwd evidence from a shared process snapshot.
///
/// The PTY child owns the pane cwd. A descendant git cwd is a candidate
/// override (Pi and similar `chdir` in a child) and must be held stable by
/// the app before it can rehome the pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneCwdEvidence {
    pub pid: u32,
    pub owner_cwd: Option<PathBuf>,
    pub owner_git_root: Option<PathBuf>,
    pub descendant_git_cwd: Option<PathBuf>,
    pub descendant_git_root: Option<PathBuf>,
}

/// Resolve CWD evidence and, optionally, process identities from one platform
/// snapshot. The optional command projection is used only when the independent
/// agent-detection deadline coincides with this CWD scan.
pub fn scan_pane_runtime(
    roots: &[u32],
    include_commands: bool,
) -> (Vec<PaneCwdEvidence>, Option<ProcessCommands>) {
    let mut cache = std::collections::HashMap::new();
    let (trees, commands) = pane_process_snapshot(roots, true, include_commands);
    let evidence = roots
        .iter()
        .map(|&root| {
            let nodes = trees.get(&root).map(Vec::as_slice).unwrap_or(&[]);
            evidence_from_tree(root, nodes, &mut cache)
        })
        .collect();
    (evidence, commands)
}

/// Resolve every pane root from **one** process-table snapshot. Git-root
/// probes are cached per directory for this scan. Call off the app loop.
#[cfg(test)]
pub fn scan_pane_cwds(roots: &[u32]) -> Vec<PaneCwdEvidence> {
    scan_pane_runtime(roots, false).0
}

fn evidence_from_tree(
    root: u32,
    nodes: &[(u32, u16)],
    cache: &mut std::collections::HashMap<String, Option<PathBuf>>,
) -> PaneCwdEvidence {
    let mut owner_cwd = None;
    let mut best_git: Option<(u16, PathBuf, PathBuf)> = None;
    for &(pid, depth) in nodes {
        let Some(cwd) = process_cwd(pid).filter(|cwd| !is_system_cwd(cwd)) else {
            continue;
        };
        if depth == 0 {
            owner_cwd = Some(cwd.clone());
        }
        if let Some(git_root) = cached_git_root(&cwd, cache) {
            if best_git
                .as_ref()
                .is_none_or(|(best_depth, _, _)| depth >= *best_depth)
            {
                best_git = Some((depth, cwd, git_root));
            }
        }
    }
    if owner_cwd.is_none() {
        owner_cwd = process_cwd(root).filter(|cwd| !is_system_cwd(cwd));
    }
    let owner_git_root = owner_cwd
        .as_ref()
        .and_then(|cwd| cached_git_root(cwd, cache));
    let (descendant_git_cwd, descendant_git_root) = match best_git {
        Some((_, cwd, git_root)) => (Some(cwd), Some(git_root)),
        None => (None, None),
    };
    PaneCwdEvidence {
        pid: root,
        owner_cwd,
        owner_git_root,
        descendant_git_cwd,
        descendant_git_root,
    }
}

fn cached_git_root(
    cwd: &Path,
    cache: &mut std::collections::HashMap<String, Option<PathBuf>>,
) -> Option<PathBuf> {
    let key = path_key(cwd);
    if let Some(hit) = cache.get(&key) {
        return hit.clone();
    }
    let root = git_root(cwd);
    cache.insert(key, root.clone());
    root
}

fn is_system_cwd(cwd: &Path) -> bool {
    let key = path_key(cwd);
    key == "c:\\windows"
        || key.starts_with("c:\\windows\\")
        || key.starts_with("c:\\program files")
        || key.starts_with("c:\\programdata")
}

/// Nearest `.git` dir or worktree `.git` file in `cwd` or any ancestor.
/// Filesystem probe only — no `git` subprocess — so a home folder like
/// `C:\Users\Administrator` cannot block the UI thread on `git rev-parse`.
pub fn git_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(|dir| dir.to_path_buf())
}

/// Raise the OS timer resolution so the event loop's timed waits (`recv_timeout`,
/// `thread::sleep`) actually run at their intended cadence. Windows' default
/// scheduler tick is ~15.6 ms, which quantizes those waits and makes the render
/// loop laggy + jittery (typing in a pane feels delayed); this drops it to 1 ms
/// while the guard is held. A no-op on Unix (already sub-millisecond). Hold the
/// returned guard for the whole process lifetime.
#[must_use]
pub fn high_res_timer() -> TimerGuard {
    #[cfg(windows)]
    // SAFETY: `timeBeginPeriod` only sets a global timer-resolution hint.
    unsafe {
        timeBeginPeriod(1);
    }
    TimerGuard
}

pub struct TimerGuard;

impl Drop for TimerGuard {
    fn drop(&mut self) {
        #[cfg(windows)]
        // SAFETY: pairs 1:1 with the `timeBeginPeriod(1)` in `high_res_timer`.
        unsafe {
            timeEndPeriod(1);
        }
    }
}

#[cfg(windows)]
#[link(name = "winmm")]
extern "system" {
    fn timeBeginPeriod(u_period: u32) -> u32;
    fn timeEndPeriod(u_period: u32) -> u32;
}

/// Is `url` safe to hand to the OS URL handler (docs/58)?
///
/// **Only `http` and `https`.** The text comes from whatever is running in a
/// pane, so a click ends at the system handler for whatever scheme it names, and
/// the interesting schemes there are all the dangerous ones. This is a
/// whitelist, not a blacklist, so a scheme nobody thought of is refused rather
/// than allowed.
///
/// Also rejects anything with a control character or whitespace: a URL is one
/// argv entry, and a newline in it has no legitimate reason to be there.
pub fn is_openable_url(url: &str) -> bool {
    let rest = match url.split_once("://") {
        Some(("http", rest)) | Some(("https", rest)) => rest,
        _ => return false,
    };
    !rest.is_empty()
        && !rest.starts_with('/')
        && !url.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// Hand `url` to the OS URL handler: `open` (macOS), `xdg-open` and friends
/// (Linux), `rundll32` (Windows).
///
/// Passed as a **separate argv entry**, never interpolated into a shell command,
/// so a URL containing shell metacharacters is inert. Callers must have cleared
/// it through [`is_openable_url`] first. Detached and never waited on, so a
/// browser cold-start cannot stall the event loop.
pub fn open_url(url: &str) {
    use std::process::{Command, Stdio};
    if !is_openable_url(url) {
        return;
    }
    let openers: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("open", &[])]
    } else if cfg!(target_os = "windows") {
        // `rundll32 url.dll,FileProtocolHandler` avoids `cmd /C start`, whose
        // first quoted argument is swallowed as a window title and which would
        // put the URL through the shell.
        &[("rundll32", &["url.dll,FileProtocolHandler"])]
    } else {
        &[("xdg-open", &[]), ("gio", &["open"]), ("wslview", &[])]
    };
    for (cmd, args) in openers {
        if no_window(
            Command::new(cmd)
                .args(*args)
                .arg(url)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        )
        .spawn()
        .is_ok()
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    #[test]
    fn process_start_marker_is_stable_for_the_current_process() {
        let pid = std::process::id();
        let first = super::process_start_marker(pid).expect("supported platform marker");
        let second = super::process_start_marker(pid).expect("same live process marker");
        assert_eq!(first, second);
        assert!(!first.is_empty());
    }

    /// The hidden-window flag must not break output capture: a command routed
    /// through [`no_window`] still runs and still reports its exit code. On
    /// Windows that is the whole contract (no window, same result); elsewhere
    /// the helper is a no-op and this pins that it stays one.
    #[test]
    fn no_window_keeps_the_command_working() {
        let mut cmd = if cfg!(windows) {
            let mut c = std::process::Command::new("cmd");
            c.args(["/C", "echo captured & exit /b 3"]);
            c
        } else {
            let mut c = std::process::Command::new("sh");
            c.args(["-c", "printf captured; exit 3"]);
            c
        };
        let output = super::no_window(&mut cmd).output().expect("spawns");
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "captured");
    }

    #[cfg(unix)]
    #[test]
    fn process_tree_finds_this_process_and_its_children() {
        // Our own pid must resolve, with its full command line intact.
        let me = std::process::id();
        let tree = super::process_tree(me);
        assert!(!tree.is_empty(), "the root process itself is listed");
        let root = &tree[0];
        assert_eq!(root.pid, me);
        assert_eq!(root.depth, 0);
        assert!(!root.command.is_empty(), "the command line is captured");

        // A child shows up nested under it, with its arguments unabridged —
        // the whole point of reading this from the OS instead of the screen.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let tree = super::process_tree(me);
        let found = tree
            .iter()
            .find(|p| p.pid == child.id())
            .expect("the child is in the tree");
        assert!(found.depth >= 1, "the child nests under us");
        assert!(
            found.command.contains("sleep") && found.command.contains("30"),
            "full argv, not truncated: {:?}",
            found.command
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn pane_runtime_scan_projects_cwd_and_commands_from_one_snapshot() {
        let pid = std::process::id();
        let (evidence, commands) = super::scan_pane_runtime(&[pid], true);
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].pid, pid);
        assert!(
            evidence[0].owner_cwd.is_some(),
            "the current process cwd is visible"
        );
        let commands = commands.expect("the process table is supported");
        assert!(
            commands.get(&pid).is_some_and(|items| !items.is_empty()),
            "the same snapshot includes the root command"
        );

        let (cwd_only, commands) = super::scan_pane_runtime(&[pid], false);
        assert_eq!(cwd_only.len(), 1);
        assert!(commands.is_none(), "command projection is demand-driven");
    }

    #[cfg(unix)]
    #[test]
    fn unix_stoppable_pid_rejects_self_and_missing() {
        assert!(!super::is_stoppable_luvus_pid(0));
        assert!(!super::is_stoppable_luvus_pid(std::process::id()));
    }

    #[cfg(unix)]
    #[test]
    fn unix_stoppable_pid_accepts_a_luvus_executable_with_arguments() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("stoppable-pid-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("test executable directory");
        let executable = dir.join("luvus");
        let _ = std::fs::remove_file(&executable);
        std::fs::copy("/bin/sleep", &executable).expect("luvus-named executable");
        let mut child = std::process::Command::new(&executable)
            .arg("30")
            .spawn()
            .expect("spawn luvus-named process");

        let mut stoppable = false;
        for _ in 0..20 {
            if super::is_stoppable_luvus_pid(child.id()) {
                stoppable = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(&executable);
        let _ = std::fs::remove_dir(&dir);
        assert!(
            stoppable,
            "a live luvus executable must pass the kill guard"
        );
    }

    #[cfg(unix)]
    #[test]
    fn force_terminate_kills_a_setsid_child() {
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new("sleep");
        command
            .arg("30")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = command.spawn().expect("spawn sleep");
        super::force_terminate(child.id()).expect("kill setsid child");
        let status = child.wait().expect("reap sleep");
        assert!(!status.success());
    }

    #[test]
    fn run_then_interactive_covers_shell_families() {
        // POSIX shells must start normally and receive the command through their
        // PTY so profile-managed executables are available.
        assert!(super::shell_run_then_interactive("/bin/zsh", "claude --resume 'abc'").is_none());
        assert!(super::shell_run_then_interactive("/bin/bash", "x").is_none());
        assert!(super::shell_run_then_interactive("/usr/bin/fish", "x").is_none());
        // PowerShell: -NoExit -Command cmd.
        let ps = super::shell_run_then_interactive("pwsh.exe", "codex resume 'a'").unwrap();
        assert_eq!(ps[1], "-NoExit");
        assert_eq!(ps[3], "codex resume 'a'");
        // Unrecognised families (and quoted paths) fall back to typing.
        assert!(super::shell_run_then_interactive("cmd.exe", "x").is_none());
        assert!(super::shell_run_then_interactive("/opt/o'dd/zsh", "x").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn shell_override_is_honored() {
        // Use a real shell so any concurrent pane spawn still succeeds.
        std::env::set_var("LUVUS_SHELL", "/bin/sh");
        // The override wins over any choice (even an explicit one).
        assert_eq!(super::resolve_shell("default"), "/bin/sh");
        assert_eq!(super::resolve_shell("zsh"), "/bin/sh");
        std::env::remove_var("LUVUS_SHELL");
    }

    #[test]
    fn same_path_ignores_verbatim_prefix_and_trailing_separator() {
        use std::path::Path;
        // The `\\?\` prefix `canonicalize` returns names the same folder.
        assert!(super::same_path(
            Path::new(r"\\?\C:\proj"),
            Path::new(r"C:\proj")
        ));
        // A trailing separator is not a different folder.
        assert!(super::same_path(
            Path::new("/work/app/"),
            Path::new("/work/app")
        ));
        // ...but a bare root must not collapse to the empty path.
        assert!(!super::same_path(Path::new("/"), Path::new("")));
        // Genuinely different folders still differ.
        assert!(!super::same_path(
            Path::new("/work/app"),
            Path::new("/work/api")
        ));
        assert!(!super::same_path(
            Path::new("/work/app"),
            Path::new("/work/app2")
        ));
    }

    #[cfg(windows)]
    #[test]
    fn same_path_folds_case_and_separators_on_windows() {
        use std::path::Path;
        // Windows paths are case-insensitive; `PathBuf` comparison is not.
        assert!(super::same_path(
            Path::new(r"C:\Users\Riz\proj"),
            Path::new(r"c:\users\riz\proj")
        ));
        // Windows accepts `/` as a separator.
        assert!(super::same_path(
            Path::new("C:/proj"),
            Path::new(r"C:\proj")
        ));
        // A bare drive root keeps its separator rather than collapsing.
        assert!(super::same_path(Path::new(r"C:\"), Path::new(r"c:\")));
        assert!(!super::same_path(Path::new(r"C:\"), Path::new(r"D:\")));
    }

    #[cfg(unix)]
    #[test]
    fn same_path_stays_case_sensitive_on_unix() {
        use std::path::Path;
        // Unix filesystems can be case-sensitive, so folding case here would
        // wrongly merge two real, distinct folders.
        assert!(!super::same_path(
            Path::new("/work/App"),
            Path::new("/work/app")
        ));
    }

    /// The whitelist is the security boundary for docs/58: this text comes from
    /// whatever is running in a pane, and a click ends at the OS handler for
    /// whatever scheme it names. Anything but http/https must be refused.
    #[test]
    fn only_http_and_https_urls_are_openable() {
        for ok in [
            "https://luvus.dev",
            "http://localhost:3000/x?y=1#z",
            "https://user:pw@example.com/a(b)",
        ] {
            assert!(super::is_openable_url(ok), "{ok:?} should open");
        }
        for bad in [
            // Scheme handlers that run code or reach the filesystem.
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,<script>x</script>",
            "vscode://file/etc/passwd",
            "smb://host/share",
            "ssh://host",
            // Not a URL at all.
            "luvus.dev",
            "https://",
            "https:///no-host",
            "",
            // Case tricks: the check is on the exact scheme, not a prefix match.
            "HTTPS://luvus.dev",
            "xhttps://luvus.dev",
            // Whitespace and control characters have no business in one argv entry.
            "https://a b.dev",
            "https://a\nb.dev",
            "https://a\u{7}b.dev",
        ] {
            assert!(!super::is_openable_url(bad), "{bad:?} must be refused");
        }
    }

    #[cfg(windows)]
    #[test]
    fn shell_choices_have_labels() {
        // Every offered choice resolves to a non-empty label and command.
        for (keyword, label) in super::shell_choices() {
            assert!(!label.is_empty());
            assert_eq!(super::shell_label(keyword), *label);
        }
        // An unknown keyword falls back to itself.
        assert_eq!(super::shell_label("nu"), "nu");
    }

    #[test]
    fn git_root_walks_every_ancestor() {
        let root = std::env::temp_dir().join(format!(
            "luvus-git-deep-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let mut nested = root.clone();
        for i in 0..12 {
            nested = nested.join(format!("d{i}"));
        }
        std::fs::create_dir_all(&nested).expect("nested dirs");
        std::fs::create_dir_all(root.join(".git")).expect("git root");
        let found = super::git_root(&nested).expect("uncapped ancestor walk");
        assert!(
            super::same_path(&found, &root),
            "git_root={found:?} repo={root:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn git_root_recognizes_worktree_git_file() {
        let root = std::env::temp_dir().join(format!(
            "luvus-git-wt-file-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let gitdir = root.join("main.git");
        let worktree = root.join("linked");
        std::fs::create_dir_all(&gitdir).expect("gitdir");
        std::fs::create_dir_all(&worktree).expect("worktree");
        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/feat\n").expect("HEAD");
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .expect("worktree git file");
        let found = super::git_root(&worktree).expect("worktree .git file");
        assert!(
            super::same_path(&found, &worktree),
            "git_root={found:?} worktree={worktree:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
