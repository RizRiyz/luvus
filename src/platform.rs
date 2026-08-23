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
    if let Some(s) = crate::compat::inherited("LUVUS_SHELL", "BOHAY_SHELL") {
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

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
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

/// Process identities running under each of `roots` (the root's own included),
/// from one platform snapshot: command lines on Unix and executable names on
/// Windows. This batched form lets agent detection cover every pane without one
/// process-table operation per pane. `None` means the platform cannot tell.
#[cfg(unix)]
pub fn descendant_commands(roots: &[u32]) -> Option<std::collections::HashMap<u32, Vec<String>>> {
    use std::collections::{HashMap, HashSet};
    let (cmd, children) = ps_table()?;
    let mut out: HashMap<u32, Vec<String>> = HashMap::new();
    for &root in roots {
        let mut found = Vec::new();
        let mut seen = HashSet::new();
        let mut stack = vec![root];
        while let Some(pid) = stack.pop() {
            // Same bounds as `process_tree`: a visited set survives pid reuse,
            // and the cap stops a pathological tree from being unbounded work.
            if !seen.insert(pid) || found.len() >= 64 {
                continue;
            }
            if let Some(c) = cmd.get(&pid) {
                found.push(c.clone());
            }
            if let Some(kids) = children.get(&pid) {
                stack.extend(kids.iter().copied());
            }
        }
        out.insert(root, found);
    }
    Some(out)
}

#[cfg(windows)]
pub fn descendant_commands(roots: &[u32]) -> Option<std::collections::HashMap<u32, Vec<String>>> {
    windows::descendant_commands(roots)
}

#[cfg(not(any(unix, windows)))]
pub fn descendant_commands(_roots: &[u32]) -> Option<std::collections::HashMap<u32, Vec<String>>> {
    None
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
        if Command::new(cmd)
            .args(*args)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
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
}
