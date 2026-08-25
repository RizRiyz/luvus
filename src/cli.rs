//! CLI client (M4): `luvus pane …` / `luvus ping` / `luvus events` connect to
//! the session socket, send one JSON request, and print the reply. See docs/08.

use std::io::{BufReader, Write};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// Returns true if `argv[1]` is a CLI noun we handle (so `main` should not
/// launch the TUI).
pub fn is_cli(args: &[String]) -> bool {
    matches!(
        args.get(1).map(String::as_str),
        Some(
            "ping"
                | "pane"
                | "node"
                | "workspace"
                | "tab"
                | "agent"
                | "bar"
                | "ui"
                | "events"
                // Reserved so the removed, unreleased command family fails as
                // an unknown command instead of accidentally opening the TUI.
                | "api"
                | "uhp"
                | "socket"
                | "module"
                | "theme"
                | "git"
                | "diff"
                | "files"
                | "worktree"
                | "task"
                | "lease"
                | "wait"
                | "search"
                | "help"
                | "doctor"
                | "update"
                | "skill"
                | "session"
        )
    )
}

const USAGE: &str = "\
luvus: Mission control for your AI coding agents

Usage:
  luvus [--session <name>]               Launch or attach to the TUI
  luvus [--session <name>] <command>     Control a local session
  luvus --remote <host> [ssh args]       Attach to a remote session
  luvus help all                         Show every command and option

Commands:
  workspace    Open, organize, and switch projects
  tab          Create, reorder, rename, and close tabs
  pane         Split, move, focus, run, inspect, and close panes
  agent        Start, fork, message, inspect, and resume coding agents
  files        Browse and open workspace files
  git          Inspect repository state and open the Git UI
  diff         Review Git diffs, notes, and agent feedback
  worktree     Create, open, list, and remove Git worktrees
  task         Coordinate work across multiple coding agents
  lease        Reserve file paths for active tasks
  module       Find, install, configure, and run extensions
  theme        List, create, validate, install, and select themes
  bar          Publish and arrange top and bottom status widgets
  ui           Configure sidebars, docks, and notifications
  session      List, attach, stop, and delete server sessions
  server       Inspect and manage the selected background server
  integration  Manage agent session-resume integrations
  skill        Enable, inspect, show, or remove the bundled agent skill
  wait         Wait for pane output or an agent state
  search       Search across pane scrollback
  events       Stream live status changes
  uhp          Discover and use Universal Harness Protocol 1.0
  attach       Open the TUI focused on one pane
  doctor       Check optional external tools
  update       Check for and install a newer Luvus release
  ping         Check whether the selected server responds

Examples:
  luvus agent list                       See every active coding agent
  luvus pane split --down                Add a pane below the focused pane
  luvus workspace open .                 Open the current project
  luvus session attach docs              Start or open a named session
  luvus --session docs agent list        Control a session from another terminal

Options:
  --session <name>                       Target a named server session
  --remote <host> [ssh args]             Attach through SSH
  --version, -V                          Print the version
  --help, -h                             Show this help

Help:
  luvus help all                         Complete CLI reference
  luvus help <topic> [command]           Focus on one area or command
  https://luvus.dev/docs/reference/cli/  Online reference
";

const DETAILED_USAGE: &str = "\
luvus: Mission control for your AI coding agents

usage: luvus <command> [args]

  (no args)            launch / attach the TUI
  --session <name>     target one named server session
  --version, -V        print the version
  --help, -h           show compact help
  help [all|<topic> [command]]  show compact, complete, or focused help
  doctor               check optional external tools (git, gh, …)
  update               check for and install a newer Luvus release
  ping                 check the server

workspaces:
  workspace list             list workspaces
  workspace new              create a workspace in the current directory
  workspace open <path>      open <path> as a workspace (or focus it if already open)
  workspace focus <i>        focus workspace i (0-based)
  workspace rename <i> <name>  rename workspace i without changing its folder
  workspace pin <i>          pin workspace i (0-based) in the sidebar
  workspace unpin <i>        unpin workspace i (0-based)
  workspace close [<i>]      close a workspace (default: active)

tabs:
  tab list                   list tabs in the current workspace
  tab new                    new tab
  tab focus <n>              focus tab n (1-based)
  tab move <from> <to>       move a tab to an exact position (1-based)
  tab move left|right        move the active tab one position (--tab N targets one)
  tab swap <first> <second>  exchange two tab positions (1-based)
  tab rename <name>          name a tab (--tab N to target one; empty clears it)
  tab close [<n>]            close a tab (default: active)

panes / agents:
  pane list                  list panes and read-only history metrics in the current tab
  pane split [<id>] [--down] [--no-focus]   split a pane (default: side by side)
  pane focus <id>            focus a pane (jumps to its workspace/tab)
  pane move [<id>] (--tab <n> | --new-tab)  move a pane within its workspace
  pane run [<id>] <cmd...>   run a command in a pane
  pane send [<id>] <text>    send raw text to a pane
  pane read [<id>]           print a pane's recent output
  pane status [<id>]         print a pane's agent status and history metrics (any workspace)
  pane processes [<id>]      list cached executable identities without exposing arguments
  pane name <name>           name a pane so you can mention it (--pane <id>; --clear)
  pane close [<id>]          close a pane
  agent list                 list every agent across all workspaces/tabs
  agent start <name> --kind <k> [--pane <id> | --anchor <id>] [--down] [--timeout <s>] [-- <args>]
                             spawn beside an anchor or reuse a pane, wait until ready, name it
  agent fork <target> [--name <alias>] [--no-focus]
                             fork a supported agent's session into a sibling pane
  agent name <name>          alias the current agent, same as pane name (--clear to drop)
  agent prompt <target> <text> [--wait] [--until STATE] [--timeout <s>]
                             atomically prompt and optionally wait (send is an alias)
  agent send <target> <text> [--wait] [--until STATE] [--timeout <s>]
                             compatibility alias for agent prompt
  agent keys <target> <key>...   send control keys (enter, esc, ctrl+c, up, …)
  agent read <target> [--lines N] [--source visible|recent]   print an agent's output
  agent get <target>         one agent's live info (pane, name, kind, status, cwd)
  agent explain <target>     show identity/state evidence and active authority
  agent report [<pane>] --source <id> --kind <agent> --status <state>
                             publish a leased authoritative state (integration API)
  agent release [<pane>] --source <id>   release that integration authority
  agent sessions             list resumable sessions found on disk
  agent resume <id>          reopen a resumable session into a pane
  skill enable               install the bundled skill in detected agent hosts
  skill status               show the bundled release and installation details
  skill disable              remove unchanged Luvus-managed installations
  skill show                 print the bundled, version-matched SKILL.md
  wait output <id> --match <text> [--timeout <s>]    block until output appears
  wait agent-status <id> --status done|blocked|working|idle [--timeout <s>]
  attach <id>                open the TUI into a single fullscreen pane

search:
  search <text...> [--case]  find text across every pane's scrollback (docs/63);
                             --case is case-sensitive; returns matches as JSON
  search --fuzzy <query...> [--scope all|navigate|files|output] [--all-sessions]
                           [--limit <1-200>] [--case] [--json]
                             rank navigation, file paths, and retained output;
                             legacy search stays exact unless --fuzzy is passed

themes:
  theme list [--json]       list built-in, installed, and virtual themes
  theme path                print/create the shared themes directory
  theme init <id> [--extends <id>]   write an editable TOML starter
  theme validate <path> [--strict] [--json]   validate without installing
  theme install <source> [--yes]     install a local file, HTTPS URL, GitHub repo, or community/<id>
  theme use <id>            select and persist a registered theme
  theme uninstall <id>      remove an inactive local theme
  theme reload              rescan installed themes in the selected server

bars:
  bar list                   list declared Luvus Bar widgets and live content
  bar push --id <id> [--region top-right|bottom-right] --content <json>
                             publish validated live widget segments;
                             --content-file, --compact-content, --text and --state supported
  bar move --id <id> --region top-right|bottom-right|off
  bar remove --id <id>       clear live widget content, preserving placement

appearance:
  ui sidebar [--side left|right] --width <n>     set a sidebar's width (columns)
  ui sidebar [--side left|right] --hide|--show   toggle a sidebar
  ui dock list               list docks and which side each is on
  ui dock move --id <id> --side left|right       place a dock on a side
  ui dock push --id <id> [--title <t>] [--side left|right] [--rows <json>]
                             feed a module's sidebar dock its rows (JSON array,
                             or piped on stdin). See docs/29 + the website
  ui notification push --text <text> [--level info|success|warning|error]
  ui notification clear [--dedupe-key <key>]
  ui toast <text>            flash a one-line message in the UI

modules (extensions):
  module search [<query>]    find modules published to the `luvus-module` GitHub topic
  module list                list installed modules
  module info <id>           show a module's actions / panes / events / source
  module link <path>         register a local module dir (--disabled to skip enabling)
  module install <owner>/<repo>[/sub] [--ref REF] [--yes]   install from GitHub
  module unlink <id>         remove a module from the registry
  module uninstall <id>      unlink + delete a git-installed module's checkout
  module enable <id> | disable <id>
                             <id> is a module id or the owner/repo it came from
  module actions             list every action across modules
  module run <id> <action>   invoke a module action (captures + logs output)
  module pane open <id> <entrypoint> [--placement split|overlay|tab]
  module pane focus <pane> | close <pane>
  module log [<id>]          tail module command logs (--limit N)
  module config-dir <id>     print/create a module's config dir
  module settings <id>       list a module's declared settings and values
  module settings <id> <key> [<value>]   read / write one setting

git:
  git status                 branch, ahead/behind, working tree of the current workspace
  git branches               local branches with tracking
  git log [--limit N]        recent commits
  git open [<workspace>]     open the git tab for a workspace
  files tree                 print the FILES tree of the active node
  files open <path> [--target pane|tab|preview]   open a file in a view
  files reveal <path>        expand the tree to a path
  files refresh              re-read the tree from disk

diff review:
  diff list [--layer staged|worktree|untracked|conflict]   list exact diff layers
  diff open [<path>] [--layer <layer>] [--view auto|split|stack]
                             [--placement preview|pane|tab]
  diff get <path> [--layer <layer>] [--include-patch]   inspect a bounded semantic diff
  diff refresh               refresh the shared FILES and DIFF index
  diff note add --file <path> (--old-line N|--new-line N) --body <text>
                             [--end-line N] [--kind question|issue|suggestion|praise]
  diff note list [--file <path>] [--state open|resolved|outdated|orphaned]
  diff note edit <id> --body <text>
  diff note resolve|reopen <id>
  diff note remove <id> --yes
  diff note send --to <agent> [<id>...] [--all-open]

worktrees:
  worktree list              list the current repo's worktrees
  worktree create <branch>   create a worktree + workspace for <branch>
  worktree open <path>       open an existing worktree as a workspace
  worktree remove <path>     remove a worktree (its branch is kept)

orchestration (multiple agents on one project, docs/22):
  task add \"<title>\" [--paths <glob>...] [--dep <id>...] [--gate <cmd>]
  task list                  list all tasks + their status/assignee
  task get <id>              show one task
  task claim <id>            claim a task for this pane (deps must be done)
  task next [--start] [--agent <cmd>]   claim the next ready task (--start spawns
                             an isolated worker), for an agent loop draining the queue
  task start <id> [--branch <b>] [--agent <cmd>]   spawn an isolated worker:
                             a git worktree + pane, auto-claimed and path-leased
  task heartbeat <id> --context <0..1>   report context usage (blocks done at >85%)
  task update <id> [--status <s>] [--output <o>] [--note <n>]
  task done <id>             mark done + release its leases
  task merge <id>            integrate the task's branch into luvus/integration
                             (isolated worktree, conflicts block the task)
  task release <id>          return a claimed task to the queue
  task delete <id>           remove a task (release/finish an active one first)
  lease acquire <glob>... --task <id>   reserve file paths (denied if they overlap)
  lease release <id>         release a lease
  lease list                 list active path leases

events:
  events                     stream live status changes

universal harness protocol:
  uhp capabilities          print live methods, contracts, limits, and protocol identity
  uhp schema                print the complete installed UHP JSON Schema bundle
  uhp snapshot              print a fenced session snapshot for harness bootstrap
  uhp events                stream sequenced UHP events
  uhp proxy                 forward one JSON request from stdin to the selected server

sessions:
  session list [--json]      list default and named server sessions
  session attach <name>      start or attach the named session
  session stop <name> [--json]    stop only the named session and its panes
  session delete <name> [--json]  delete a stopped named session

remote:
  --remote <host> [ssh args] attach to a luvus session on <host> over plain ssh

server:
  server status              is the server running, and what version
  server start               start the background server if it isn't up
  server stop                stop the server (and all panes)
  server restart             stop + start (load a newly-installed binary)
  server update-manifest     fetch the latest agent-detection rules from luvus.dev
                             (applies live if the server is up; else on next start)
  integration install|uninstall <claude|copilot|codex|opencode|kimi|grok>
                             add/remove luvus's session-resume hook (uninstall
                             removes only luvus's hook, never the agent)
";

pub fn run(args: &[String]) -> Result<i32> {
    if args.get(1).map(String::as_str) == Some("help") {
        return match args.get(2).map(String::as_str) {
            None => {
                print!("{USAGE}{HELP_BUG}");
                Ok(0)
            }
            Some("all") if args.len() == 3 => {
                print!("{DETAILED_USAGE}{HELP_BUG}");
                Ok(0)
            }
            Some(topic) if matches!(args.len(), 3 | 4) => {
                let command = args.get(3).map(String::as_str);
                if write_topic_help(std::io::stdout().lock(), topic, command)? {
                    Ok(0)
                } else {
                    eprintln!("unknown help topic `{topic}`. Run `luvus --help` for the list.");
                    Ok(2)
                }
            }
            _ => {
                eprintln!("usage: luvus help [all|<topic> [command]]");
                Ok(2)
            }
        };
    }
    if let Some((topic, command)) = command_help_request(args) {
        write_topic_help(std::io::stdout().lock(), topic, command)?;
        return Ok(0);
    }
    if args.get(1).map(String::as_str) == Some("uhp") && args.len() == 2 {
        write_topic_help(std::io::stdout().lock(), args[1].as_str(), None)?;
        return Ok(0);
    }
    if args.get(1).map(String::as_str) == Some("skill") {
        return skill_cmd(&args[2.min(args.len())..]);
    }
    if args.get(1).map(String::as_str) == Some("session") {
        return session_cmd(&args[2.min(args.len())..]);
    }
    if args.get(1).map(String::as_str) == Some("theme") {
        return theme_cmd(&args[2.min(args.len())..]);
    }
    if args.get(1).map(String::as_str) == Some("uhp")
        && args.get(2).map(String::as_str) == Some("schema")
    {
        if args.len() != 3 {
            return Err(anyhow!("usage: luvus uhp schema"));
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&crate::api::schema_bundle())?
        );
        return Ok(0);
    }
    if args.get(1).map(String::as_str) == Some("uhp")
        && args.get(2).map(String::as_str) == Some("proxy")
    {
        if args.len() != 3 {
            return Err(anyhow!("usage: luvus uhp proxy"));
        }
        return uhp_proxy();
    }
    // Explicit update requests are local and never require a running server.
    if args.get(1).map(String::as_str) == Some("update") {
        return crate::update::run_cli(&args[2.min(args.len())..]);
    }
    // `doctor` is a local environment check — no server needed.
    if args.get(1).map(String::as_str) == Some("doctor") {
        return Ok(doctor());
    }
    // `module install` clones + builds locally (with a confirm prompt), then
    // registers over the socket — it isn't a plain request/response.
    if args.get(1).map(String::as_str) == Some("module")
        && args.get(2).map(String::as_str) == Some("install")
    {
        return module_install(args);
    }
    // `module search` is a read-only GitHub lookup — no server involved.
    if args.get(1).map(String::as_str) == Some("module")
        && args.get(2).map(String::as_str) == Some("search")
    {
        return module_search(args);
    }
    // `wait` (docs/18 WA-1) is a client-side poll/stream loop, not a one-shot
    // request — it exits 0 on the condition, 2 on timeout.
    if args.get(1).map(String::as_str) == Some("wait") {
        return wait_cmd(args);
    }
    // Agent prompt/send and start are server-owned workflows, not plain
    // dispatch calls; their one connection stays parked for the optional wait.
    if args.get(1).map(String::as_str) == Some("agent")
        && matches!(args.get(2).map(String::as_str), Some("prompt" | "send"))
    {
        return agent_send_cmd(args);
    }
    if args.get(1).map(String::as_str) == Some("agent")
        && args.get(2).map(String::as_str) == Some("start")
    {
        return agent_start_cmd(args);
    }
    let (method, params) = parse(args)?;
    let path = crate::persist::cli_socket_path();
    let mut stream = crate::ipc::transport::connect(&path)
        .map_err(|_| anyhow!("no luvus server running (socket: {})", path.display()))?;

    let req = json!({ "id": "1", "method": method, "params": params });
    writeln!(stream, "{req}")?;

    let mut reader = BufReader::new(stream);
    if matches!(
        method.as_str(),
        "events.subscribe" | "terminal.backend.events.subscribe"
    ) {
        // Stream bounded events until the connection closes cleanly.
        while let Some(line) = crate::ipc::api::read_stream_frame(&mut reader)? {
            println!("{line}");
        }
        return Ok(0);
    }

    let line = crate::ipc::api::read_response_frame(&mut reader)?;
    let line = line.trim();
    // Pretty-print and set exit code on error.
    match serde_json::from_str::<Value>(line) {
        Ok(v) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&v).unwrap_or_else(|_| line.to_string())
            );
            if v.get("error").is_some() {
                return Ok(1);
            }
        }
        Err(_) => println!("{line}"),
    }
    Ok(0)
}

/// True when an explicit command help request must be handled before `main`
/// dispatches commands such as `server` and `integration` to local handlers.
pub fn is_help_request(args: &[String]) -> bool {
    args.get(1).map(String::as_str) == Some("help") || command_help_request(args).is_some()
}

fn command_help_request(args: &[String]) -> Option<(&str, Option<&str>)> {
    let topic = args.get(1)?.as_str();
    let normalized = normalize_help_topic(topic)?;
    let group_help = args.len() == 3
        && matches!(
            args.get(2).map(String::as_str),
            Some("help" | "--help" | "-h")
        );
    let command_help = args.len() > 3
        && matches!(args.last().map(String::as_str), Some("--help" | "-h"))
        && !trailing_help_is_pass_through_payload(args, normalized);
    if group_help {
        return Some((topic, None));
    }
    if command_help {
        let command = help_topic_has_subcommands(normalized).then(|| args[2].as_str());
        return Some((topic, command));
    }
    None
}

fn trailing_help_is_pass_through_payload(args: &[String], topic: &str) -> bool {
    match topic {
        "pane" => matches!(args.get(2).map(String::as_str), Some("run" | "send")),
        "agent" => matches!(
            args.get(2).map(String::as_str),
            Some("prompt" | "send" | "keys")
        ),
        // `--remote <host> [ssh args]` forwards everything after the host to SSH.
        "remote" => args.len() > 3,
        _ => false,
    }
}

fn help_topic_has_subcommands(topic: &str) -> bool {
    matches!(
        topic,
        "workspace"
            | "tab"
            | "pane"
            | "agent"
            | "files"
            | "git"
            | "diff"
            | "worktree"
            | "task"
            | "lease"
            | "module"
            | "theme"
            | "bar"
            | "ui"
            | "session"
            | "server"
            | "integration"
            | "skill"
            | "wait"
            | "uhp"
    )
}

fn normalize_help_topic(topic: &str) -> Option<&str> {
    match topic {
        "workspace" | "tab" | "pane" | "agent" | "files" | "git" | "worktree" | "task"
        | "lease" | "module" | "theme" | "bar" | "ui" | "session" | "server" | "integration"
        | "diff" | "skill" | "wait" | "search" | "events" | "uhp" | "ping" | "doctor"
        | "update" | "attach" => Some(topic),
        "node" => Some("pane"),
        "remote" | "--remote" => Some("remote"),
        _ => None,
    }
}

const HELP_BUG: &str = r#"
\   /
 \_/    If you are an AI, read this:
(o_o)   https://luvus.dev/agent-readme.md
/|_|\
"#;

fn write_topic_help(
    mut output: impl Write,
    requested: &str,
    command: Option<&str>,
) -> std::io::Result<bool> {
    let Some(topic) = normalize_help_topic(requested) else {
        return Ok(false);
    };
    if topic == "session" {
        if let Some(command) = command {
            writeln!(output, "Usage: luvus session <command>\n")?;
            if !write_topic_rows(&mut output, SESSION_USAGE, topic, Some(command))? {
                output.write_all(SESSION_USAGE.as_bytes())?;
            }
        } else {
            output.write_all(SESSION_USAGE.as_bytes())?;
        }
        output.write_all(HELP_BUG.as_bytes())?;
        return Ok(true);
    }

    let (usage, section) = match topic {
        "workspace" => (
            "luvus workspace <command> [args]",
            detailed_section("workspaces:\n", "\ntabs:\n"),
        ),
        "tab" => (
            "luvus tab <command> [args]",
            detailed_section("tabs:\n", "\npanes / agents:\n"),
        ),
        "pane" => (
            "luvus pane <command> [args]",
            detailed_section("panes / agents:\n", "\nsearch:\n"),
        ),
        "agent" => (
            "luvus agent <command> [args]",
            detailed_section("panes / agents:\n", "\nsearch:\n"),
        ),
        "skill" => (
            "luvus skill <enable|status|disable|show>",
            detailed_section("panes / agents:\n", "\nsearch:\n"),
        ),
        "wait" => (
            "luvus wait <output|agent-status> [args]",
            detailed_section("panes / agents:\n", "\nsearch:\n"),
        ),
        "attach" => (
            "luvus attach <pane-id>",
            detailed_section("panes / agents:\n", "\nsearch:\n"),
        ),
        "search" => (
            "luvus search <text...> [--case]\n       luvus search --fuzzy <query...> [--scope all|navigate|files|output] [--all-sessions] [--limit 1-200] [--case] [--json]",
            detailed_section("search:\n", "\nthemes:\n"),
        ),
        "theme" => (
            "luvus theme <list|path|init|validate|install|use|uninstall|reload> [args]",
            detailed_section("themes:\n", "\nbars:\n"),
        ),
        "bar" => (
            "luvus bar <list|push|move|remove> [args]",
            detailed_section("bars:\n", "\nappearance:\n"),
        ),
        "ui" => (
            "luvus ui <sidebar|dock|notification|toast> [args]",
            detailed_section("appearance:\n", "\nmodules (extensions):\n"),
        ),
        "module" => (
            "luvus module <command> [args]",
            detailed_section("modules (extensions):\n", "\ngit:\n"),
        ),
        "git" => (
            "luvus git <status|branches|log|open> [args]",
            detailed_section("git:\n", "\ndiff review:\n"),
        ),
        "files" => (
            "luvus files <tree|open|reveal|refresh> [args]",
            detailed_section("git:\n", "\ndiff review:\n"),
        ),
        "diff" => (
            "luvus diff <list|open|get|refresh|note> [args]",
            detailed_section("diff review:\n", "\nworktrees:\n"),
        ),
        "worktree" => (
            "luvus worktree <command> [args]",
            detailed_section(
                "worktrees:\n",
                "\norchestration (multiple agents on one project, docs/22):\n",
            ),
        ),
        "task" => (
            "luvus task <command> [args]",
            detailed_section(
                "orchestration (multiple agents on one project, docs/22):\n",
                "\nevents:\n",
            ),
        ),
        "lease" => (
            "luvus lease <acquire|release|list> [args]",
            detailed_section(
                "orchestration (multiple agents on one project, docs/22):\n",
                "\nevents:\n",
            ),
        ),
        "events" => (
            "luvus events",
            detailed_section("events:\n", "\nuniversal harness protocol:\n"),
        ),
        "uhp" => (
            "luvus uhp <capabilities|schema|snapshot|events|proxy>",
            detailed_section("universal harness protocol:\n", "\nsessions:\n"),
        ),
        "remote" => (
            "luvus [--session <name>] --remote <host> [ssh args]",
            detailed_section("remote:\n", "\nserver:\n"),
        ),
        "server" => (
            "luvus [--session <name>] server <command>",
            detailed_section_to_end("server:\n"),
        ),
        "integration" => (
            "luvus integration <install|uninstall> <agent>",
            detailed_section_to_end("server:\n"),
        ),
        "ping" => (
            "luvus [--session <name>] ping",
            "Check whether the selected server responds.\n",
        ),
        "doctor" => (
            "luvus doctor",
            "Check optional external tools used by Luvus.\n",
        ),
        "update" => (
            "luvus update",
            "Check for a newer release and install it through the detected safe update channel.\n",
        ),
        _ => unreachable!("normalized help topic"),
    };

    writeln!(output, "Usage: {usage}\n")?;
    if !write_topic_rows(&mut output, section, topic, command)? {
        output.write_all(section.as_bytes())?;
    }
    output.write_all(HELP_BUG.as_bytes())?;
    Ok(true)
}

fn write_topic_rows(
    output: &mut impl Write,
    section: &str,
    topic: &str,
    command: Option<&str>,
) -> std::io::Result<bool> {
    let noun = match topic {
        "session" => "",
        "remote" | "--remote" => "--remote",
        _ => topic,
    };
    let mut matched = false;
    let mut include_continuation = false;

    for line in section.lines() {
        let is_command_row = line.starts_with("  ")
            && line
                .as_bytes()
                .get(2)
                .is_some_and(|character| *character != b' ');
        if is_command_row {
            let row = line.trim_start();
            let rest = if noun.is_empty() {
                row
            } else if let Some(rest) = row
                .strip_prefix(noun)
                .and_then(|rest| rest.strip_prefix(' '))
            {
                rest
            } else {
                include_continuation = false;
                continue;
            };
            let syntax = rest.split("  ").next().unwrap_or(rest);
            include_continuation = command.is_none_or(|command| {
                syntax
                    .split(|character: char| character.is_whitespace() || character == '|')
                    .any(|token| token == command)
            });
            if include_continuation {
                matched = true;
                writeln!(output, "{line}")?;
            }
        } else if include_continuation && !line.trim().is_empty() {
            writeln!(output, "{line}")?;
        }
    }
    Ok(matched)
}

fn detailed_section(start: &str, end: &str) -> &'static str {
    let start = DETAILED_USAGE
        .find(start)
        .expect("help section start must exist");
    let tail = &DETAILED_USAGE[start..];
    let end = tail.find(end).expect("help section end must exist");
    &tail[..end]
}

fn detailed_section_to_end(start: &str) -> &'static str {
    let start = DETAILED_USAGE
        .find(start)
        .expect("help section start must exist");
    &DETAILED_USAGE[start..]
}

fn session_cmd(args: &[String]) -> Result<i32> {
    match args.first().map(String::as_str) {
        Some("list") => session_list(&args[1..]),
        Some("stop") => session_stop(&args[1..]),
        Some("delete") => session_delete(&args[1..]),
        Some("attach")
            if matches!(
                args.get(1).map(String::as_str),
                Some("help" | "--help" | "-h")
            ) =>
        {
            write_session_help(std::io::stdout().lock())?;
            Ok(0)
        }
        Some("attach") => {
            eprintln!("usage: luvus session attach <name>");
            Ok(2)
        }
        Some("help" | "--help" | "-h") => {
            write_session_help(std::io::stdout().lock())?;
            Ok(0)
        }
        _ => {
            write_session_help(std::io::stderr().lock())?;
            Ok(2)
        }
    }
}

const SESSION_USAGE: &str = "\
Usage: luvus session <command>

Commands:
  list [--json]         List default and named sessions
  attach <name>        Start or attach to a named session
  stop <name> [--json] Stop a named session and its panes
  delete <name> [--json] Delete a stopped named session
";

fn write_session_help(mut output: impl Write) -> std::io::Result<()> {
    output.write_all(SESSION_USAGE.as_bytes())?;
    output.write_all(HELP_BUG.as_bytes())
}

fn session_list(args: &[String]) -> Result<i32> {
    let json = match args {
        [] => false,
        [flag] if flag == "--json" => true,
        _ => return Err(anyhow!("usage: luvus session list [--json]")),
    };
    let sessions = crate::session::list_sessions()?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"sessions": sessions}))?
        );
        return Ok(0);
    }
    println!("{:<24} {:<10} directory", "name", "status");
    for session in sessions {
        println!(
            "{:<24} {:<10} {}",
            session.name,
            if session.running {
                "running"
            } else {
                "stopped"
            },
            session.session_dir
        );
    }
    Ok(0)
}

fn parse_session_name_and_json<'a>(args: &'a [String], usage: &str) -> Result<(&'a str, bool)> {
    match args {
        [name] => Ok((name, false)),
        [name, flag] if flag == "--json" => Ok((name, true)),
        _ => Err(anyhow!(usage.to_string())),
    }
}

fn session_stop(args: &[String]) -> Result<i32> {
    let (name, json_output) =
        parse_session_name_and_json(args, "usage: luvus session stop <name> [--json]")?;
    let target = match crate::session::parse_target_name(name) {
        Ok(target) => target,
        Err(message) => return session_error("invalid_session_name", &message, json_output),
    };
    match crate::session::stop_session(target.as_deref()) {
        Ok(session) => {
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({"stopped": true, "session": session}))?
                );
            } else {
                println!("stopped session {}", session.name);
            }
            Ok(0)
        }
        Err(message) => session_error("session_stop_failed", &message, json_output),
    }
}

fn session_delete(args: &[String]) -> Result<i32> {
    let (name, json_output) =
        parse_session_name_and_json(args, "usage: luvus session delete <name> [--json]")?;
    match crate::session::delete_session(name) {
        Ok(session) => {
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({"deleted": true, "session": session}))?
                );
            } else {
                println!("deleted session {}", session.name);
            }
            Ok(0)
        }
        Err(message) => session_error("session_delete_failed", &message, json_output),
    }
}

fn session_error(code: &str, message: &str, json_output: bool) -> Result<i32> {
    let error = json!({"error": {"code": code, "message": message}});
    if json_output {
        println!("{}", serde_json::to_string_pretty(&error)?);
    } else {
        eprintln!("{message}");
    }
    Ok(1)
}

fn theme_cmd(args: &[String]) -> Result<i32> {
    let subcommand = args.first().map(String::as_str).unwrap_or("list");
    let json_output = args.iter().any(|arg| arg == "--json");
    match subcommand {
        "list" => {
            if args.iter().skip(1).any(|arg| arg != "--json") {
                return Err(anyhow!("usage: luvus theme list [--json]"));
            }
            let registry = crate::theme::ThemeRegistry::load();
            let active = crate::config::load().theme;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&registry.list_json(&active))?
                );
            } else {
                for entry in registry.entries() {
                    let marker = if crate::ui::theme::canonical(&active) == entry.id {
                        '*'
                    } else {
                        ' '
                    };
                    let source = match &entry.source {
                        crate::theme::registry::ThemeSource::BuiltIn => "built-in",
                        crate::theme::registry::ThemeSource::Local { .. } => "local",
                        crate::theme::registry::ThemeSource::Virtual => "virtual",
                    };
                    println!(
                        "{marker} {:<28} {:<9} {}",
                        entry.id, source, entry.description
                    );
                    for warning in &entry.warnings {
                        println!("    warning: {warning}");
                    }
                }
                for problem in registry.problems() {
                    eprintln!("invalid {}: {}", problem.path, problem.message);
                }
            }
            Ok(if registry.problems().is_empty() { 0 } else { 1 })
        }
        "path" => {
            reject_theme_extras(args, 1, "usage: luvus theme path")?;
            println!("{}", crate::theme::ensure_themes_dir()?.display());
            Ok(0)
        }
        "init" => {
            let id = args
                .get(1)
                .filter(|arg| !arg.starts_with('-'))
                .ok_or_else(|| anyhow!("usage: luvus theme init <id> [--extends <id>]"))?;
            if !matches!(args.len(), 2 | 4)
                || (args.len() == 4 && args.get(2).map(String::as_str) != Some("--extends"))
            {
                return Err(anyhow!("usage: luvus theme init <id> [--extends <id>]"));
            }
            let parent = flag(args, "--extends");
            let path = std::path::PathBuf::from(format!("{id}.toml"));
            crate::theme::install::init(&path, id, parent.as_deref())?;
            println!("created {}", path.display());
            Ok(0)
        }
        "validate" => {
            let path = args
                .get(1)
                .filter(|arg| !arg.starts_with('-'))
                .ok_or_else(|| anyhow!("usage: luvus theme validate <path> [--strict] [--json]"))?;
            if args
                .iter()
                .skip(2)
                .any(|arg| !matches!(arg.as_str(), "--strict" | "--json"))
            {
                return Err(anyhow!(
                    "usage: luvus theme validate <path> [--strict] [--json]"
                ));
            }
            let strict = args.iter().any(|arg| arg == "--strict");
            let (file, warnings) =
                match crate::theme::install::validate_path(std::path::Path::new(path), strict) {
                    Ok(validated) => validated,
                    Err(error) if json_output => {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&json!({
                                "valid": false,
                                "error": format!("{error:#}"),
                            }))?
                        );
                        return Ok(1);
                    }
                    Err(error) => return Err(error),
                };
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "valid": true,
                        "id": file.id,
                        "display_name": file.display_name,
                        "warnings": warnings,
                    }))?
                );
            } else {
                println!("valid theme: {} ({})", file.display_name, file.id);
                for warning in warnings {
                    println!("warning: {warning}");
                }
            }
            Ok(0)
        }
        "install" => {
            let source = args
                .get(1)
                .filter(|arg| !arg.starts_with('-'))
                .ok_or_else(|| {
                    anyhow!(
                        "usage: luvus theme install <path|https-url|github-repo|community/id> [--yes]"
                    )
                })?;
            if args
                .iter()
                .skip(2)
                .any(|arg| !matches!(arg.as_str(), "--yes" | "-y"))
            {
                return Err(anyhow!(
                    "usage: luvus theme install <path|https-url|github-repo|community/id> [--yes]"
                ));
            }
            let yes = args
                .iter()
                .any(|arg| matches!(arg.as_str(), "--yes" | "-y"));
            let installed = crate::theme::install::install(source, yes)?;
            let reloaded = reload_theme_server()?;
            println!(
                "installed {} ({}) from {} to {}{}",
                installed.display_name,
                installed.id,
                installed.source,
                installed.path.display(),
                if reloaded {
                    " and reloaded the selected server"
                } else {
                    " — start or reload Luvus to use it"
                }
            );
            for warning in installed.warnings {
                println!("warning: {warning}");
            }
            Ok(0)
        }
        "use" => {
            let id = args
                .get(1)
                .filter(|arg| !arg.starts_with('-'))
                .ok_or_else(|| anyhow!("usage: luvus theme use <id>"))?;
            reject_theme_extras(args, 2, "usage: luvus theme use <id>")?;
            let registry = crate::theme::ThemeRegistry::load();
            let entry = registry
                .get(id)
                .ok_or_else(|| anyhow!("theme `{id}` is not installed"))?;
            let selected = entry.id.clone();
            match send_request("theme.use", json!({"id": selected})) {
                Ok(response) if !is_unknown_method(&response, "theme.use") => {
                    ensure_api_success(&response)?;
                    println!("using theme {}", entry.id);
                }
                Ok(_) | Err(_) => {
                    let mut config = crate::config::load();
                    config.theme = selected;
                    crate::config::save(&config);
                    println!("using theme {} — applies when Luvus starts", entry.id);
                }
            }
            Ok(0)
        }
        "uninstall" => {
            let id = args
                .get(1)
                .filter(|arg| !arg.starts_with('-'))
                .ok_or_else(|| anyhow!("usage: luvus theme uninstall <id>"))?;
            reject_theme_extras(args, 2, "usage: luvus theme uninstall <id>")?;
            let path = crate::theme::install::uninstall(id)?;
            let reloaded = reload_theme_server()?;
            println!(
                "uninstalled {id} ({}){}",
                path.display(),
                if reloaded {
                    " and reloaded the selected server"
                } else {
                    ""
                }
            );
            Ok(0)
        }
        "reload" => {
            reject_theme_extras(args, 1, "usage: luvus theme reload")?;
            let registry = crate::theme::ThemeRegistry::load();
            if !registry.problems().is_empty() {
                for problem in registry.problems() {
                    eprintln!("invalid {}: {}", problem.path, problem.message);
                }
            }
            if reload_theme_server()? {
                println!("reloaded {} themes", registry.entries().len());
            } else {
                println!(
                    "validated {} themes — start Luvus to load them",
                    registry.entries().len()
                );
            }
            Ok(if registry.problems().is_empty() { 0 } else { 1 })
        }
        "help" | "--help" | "-h" => {
            write_topic_help(std::io::stdout().lock(), "theme", None)?;
            Ok(0)
        }
        _ => Err(anyhow!(
            "usage: luvus theme <list|path|init|validate|install|use|uninstall|reload>"
        )),
    }
}

fn reject_theme_extras(args: &[String], expected: usize, usage: &str) -> Result<()> {
    if args.len() != expected {
        return Err(anyhow!(usage.to_string()));
    }
    Ok(())
}

fn is_unknown_method(response: &Value, method: &str) -> bool {
    response
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .and_then(|message| message.strip_prefix("unknown method: "))
        == Some(method)
}

fn ensure_api_success(response: &Value) -> Result<()> {
    if let Some(error) = response.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("theme request failed");
        return Err(anyhow!(message.to_string()));
    }
    Ok(())
}

fn reload_theme_server() -> Result<bool> {
    match send_request("theme.reload", json!({})) {
        Ok(response) => {
            if is_unknown_method(&response, "theme.reload") {
                return Ok(false);
            }
            ensure_api_success(&response)?;
            Ok(true)
        }
        Err(_) => Ok(false),
    }
}

/// `luvus module install owner/repo[/sub] [--ref REF] [--yes]` — clone + build
/// locally, then register over the socket (or directly if the server is down).
fn module_install(args: &[String]) -> Result<i32> {
    let spec = args
        .get(3)
        .filter(|s| !s.starts_with("--"))
        .ok_or_else(|| {
            anyhow!("usage: luvus module install owner/repo[/sub] [--ref REF] [--yes]")
        })?;
    let git_ref = flag(args, "--ref");
    let yes = args.iter().any(|a| a == "--yes" || a == "-y");

    let installed = crate::module::install::install(spec, git_ref.as_deref(), yes)?;
    let params = json!({
        "path": installed.root.display().to_string(),
        "source": installed.source,
    });
    match send_request("module.link", params) {
        Ok(v) if v.get("error").is_some() => {
            // e.g. already registered — leave the checkout but report it.
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            Ok(1)
        }
        Ok(_) => {
            println!("installed {} ({})", installed.id, installed.source);
            Ok(0)
        }
        Err(_) => {
            // Server down: write the registry directly; it loads on next start.
            register_directly(&installed)?;
            println!(
                "installed {} ({}) — start luvus to use it",
                installed.id, installed.source
            );
            Ok(0)
        }
    }
}

/// `luvus doctor` — report which optional external tools are present. The core
/// multiplexer needs none of them; this just tells a fresh install (esp. via
/// `cargo install`, which can't pull in system tools) what each missing tool
/// would unlock and how to get it. Always exits 0 — nothing here is fatal.
fn doctor() -> i32 {
    use std::process::Command;
    // Run `<cmd> <arg>` and return its first non-empty version line, if it runs.
    let probe = |cmd: &str, arg: &str| -> Option<String> {
        let out = Command::new(cmd).arg(arg).output().ok()?;
        let bytes = if !out.stdout.is_empty() {
            out.stdout
        } else {
            out.stderr // ssh prints its version to stderr
        };
        String::from_utf8_lossy(&bytes)
            .lines()
            .next()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
    };

    println!("luvus {}\n", env!("CARGO_PKG_VERSION"));
    println!("  ✓ core    the multiplexer (panes · tabs · agents) needs no external tools\n");

    // (name, cmd, version-arg, what it unlocks, required?, install hint)
    let tools = [
        (
            "git",
            "git",
            "--version",
            "git tab · worktrees",
            true,
            "https://git-scm.com  (brew install git)",
        ),
        (
            "gh",
            "gh",
            "--version",
            "GitHub PRs & issues",
            false,
            "https://cli.github.com  (brew install gh)",
        ),
        (
            "ssh",
            "ssh",
            "-V",
            "luvus --remote",
            false,
            "preinstalled on macOS/Linux",
        ),
        (
            "curl",
            "curl",
            "--version",
            "luvus module search",
            false,
            "preinstalled on macOS/Linux",
        ),
    ];

    let mut missing_git = false;
    for (name, cmd, arg, unlocks, required, hint) in tools {
        match probe(cmd, arg) {
            Some(ver) => {
                // Trim noisy version banners (e.g. curl's) to keep it scannable.
                let short: String = ver.chars().take(26).collect();
                println!("  ✓ {name:<6}{short:<28}{unlocks}");
            }
            None => {
                if required {
                    missing_git = true;
                }
                let kind = if required { "needed for" } else { "optional -" };
                println!("  ✗ {name:<6}not found · {kind} {unlocks}");
                println!("           ↳ {hint}");
            }
        }
    }

    // Whether this terminal can tell Shift+Enter from Enter. Legacy encoding
    // sends a bare CR for both, so an agent's "new line, don't submit" key only
    // works where the terminal speaks the progressive keyboard protocol.
    println!();
    match keyboard_protocol_status() {
        KeyProto::InsidePane => {
            println!("  · keys    run `luvus doctor` outside a luvus pane to test your terminal");
        }
        KeyProto::Supported => {
            println!("  ✓ keys    Shift+Enter works (terminal reports modified keys)");
        }
        KeyProto::Unsupported => {
            let is_wsl = std::env::var_os("WSL_DISTRO_NAME").is_some()
                || std::env::var_os("WSL_INTEROP").is_some();
            let in_windows_terminal = std::env::var_os("WT_SESSION").is_some();
            let (detail, action) = unsupported_key_guidance(is_wsl, in_windows_terminal);
            println!("  ! keys    Shift+Enter isn't distinguishable here · optional");
            println!("           ↳ {detail}");
            println!("             {action}");
        }
    }

    println!();
    if missing_git {
        println!("Tip: install `git` to use the git tab & worktrees. Everything else works now.");
    } else {
        println!("All set — you're good to go. ✓");
    }
    0
}

enum KeyProto {
    Supported,
    // Windows always reports Supported (native console records), so this variant
    // is only constructed off Windows.
    #[cfg_attr(windows, allow(dead_code))]
    Unsupported,
    /// Queried from inside a luvus pane, which answers for *luvus's* PTY rather
    /// than the real terminal — so the result would be misleading.
    InsidePane,
}

/// Reassure users that a missing modified-key protocol is not a Luvus failure,
/// then provide the repair that matches the terminal path they are actually
/// using. Windows Terminal gained the protocol in 1.25; older releases can
/// send Luvus's existing newline sequence with a `sendInput` binding.
fn unsupported_key_guidance(
    is_wsl: bool,
    in_windows_terminal: bool,
) -> (&'static str, &'static str) {
    match (is_wsl, in_windows_terminal) {
        (true, true) => (
            "WSL in Windows Terminal detected; all other features still work",
            "update Windows Terminal to 1.25+, or bind Shift+Enter to ESC CR",
        ),
        (true, false) => (
            "WSL detected; all other features still work",
            "use Windows Terminal 1.25+ or bind Shift+Enter to ESC CR",
        ),
        (false, _) => (
            "Luvus still works; only the modified-Enter shortcut is affected",
            "use Alt/Option+Enter or a terminal with the keyboard protocol",
        ),
    }
}

/// Ask the terminal whether it supports progressive keyboard enhancement. The
/// query needs raw mode (crossterm writes a request and reads the reply), so it
/// is enabled just for the probe and always restored.
fn keyboard_protocol_status() -> KeyProto {
    if std::env::var_os("LUVUS_ENV").is_some() {
        return KeyProto::InsidePane;
    }
    // Windows reads keys from native console records (not the keyboard protocol,
    // which `supports_keyboard_enhancement` always reports `false` for there), and
    // those records carry the SHIFT modifier — so Shift+Enter is distinguishable
    // regardless. Reporting on the protocol would wrongly say it isn't.
    #[cfg(windows)]
    {
        KeyProto::Supported
    }
    #[cfg(not(windows))]
    {
        use ratatui::crossterm::terminal;
        let raw = terminal::enable_raw_mode().is_ok();
        let supported = matches!(terminal::supports_keyboard_enhancement(), Ok(true));
        if raw {
            let _ = terminal::disable_raw_mode();
        }
        if supported {
            KeyProto::Supported
        } else {
            KeyProto::Unsupported
        }
    }
}

/// `luvus module search [<query>]` — list modules published to the
/// `luvus-module` GitHub topic. Read-only; doesn't need a running server.
fn module_search(args: &[String]) -> Result<i32> {
    let terms: Vec<&str> = args
        .get(3..)
        .unwrap_or(&[])
        .iter()
        .filter(|a| !a.starts_with("--"))
        .map(String::as_str)
        .collect();
    let query = (!terms.is_empty()).then(|| terms.join(" "));

    let hits = crate::module::discovery::search(query.as_deref())?;
    if hits.is_empty() {
        println!("No modules found in the `luvus-module` topic yet.");
        println!("Publish one by tagging a public repo with the `luvus-module` topic.");
        return Ok(0);
    }
    for h in &hits {
        println!("  ★ {:<5} {}", h.stars, h.full_name);
        if !h.description.is_empty() {
            println!("          {}", h.description);
        }
        if !h.url.is_empty() {
            println!("          {}", h.url);
        }
    }
    println!(
        "\n{} result(s). Install with:  luvus module install <owner>/<repo>",
        hits.len()
    );
    Ok(0)
}

/// What a `luvus wait …` invocation is waiting for (parsed from argv).
#[derive(Debug, PartialEq)]
enum WaitFor {
    /// `wait output <id> --match <text>`: the pane's recent output contains `text`.
    Output { needle: String },
    /// `wait agent-status <id> --status <s>`: the pane's agent reaches `status`.
    AgentStatus { status: String },
}

#[derive(Debug, PartialEq)]
struct WaitSpec {
    pane: String,
    condition: WaitFor,
    timeout: Option<f64>,
}

/// Parse `luvus wait output|agent-status <id> …` into a [`WaitSpec`] (pure, so
/// it's unit-tested). The pane id is the first numeric positional, else
/// `$LUVUS_PANE_ID`.
fn parse_wait(args: &[String]) -> Result<WaitSpec> {
    let kind = args.get(2).map(String::as_str).unwrap_or("");
    let pane = args
        .get(3)
        .filter(|s| s.parse::<u32>().is_ok())
        .cloned()
        .or_else(|| std::env::var("LUVUS_PANE_ID").ok())
        .ok_or_else(|| anyhow!("wait needs a pane id (or $LUVUS_PANE_ID)"))?;
    let timeout = match flag(args, "--timeout") {
        Some(raw) => {
            let seconds = raw
                .parse::<f64>()
                .map_err(|_| anyhow!("--timeout must be a non-negative number of seconds"))?;
            if std::time::Duration::try_from_secs_f64(seconds).is_err() || seconds > 3600.0 {
                return Err(anyhow!("--timeout must be between 0 and 3600 seconds"));
            }
            Some(seconds)
        }
        None => None,
    };
    let condition = match kind {
        "output" => WaitFor::Output {
            needle: flag(args, "--match").ok_or_else(|| {
                anyhow!("usage: luvus wait output <id> --match <text> [--timeout <s>]")
            })?,
        },
        "agent-status" => WaitFor::AgentStatus {
            status: flag(args, "--status").ok_or_else(|| {
                anyhow!("usage: luvus wait agent-status <id> --status done|blocked|working|idle [--timeout <s>]")
            })?,
        },
        _ => return Err(anyhow!("usage: luvus wait output|agent-status <id> …")),
    };
    Ok(WaitSpec {
        pane,
        condition,
        timeout,
    })
}

/// `luvus wait …` — block until the condition holds (exit 0) or the timeout
/// elapses (exit 2). `wait output` is answered server-side: the connection
/// stays open and the server replies the moment the pane's output matches,
/// so the CLI never polls (docs/81). Older servers answer `unknown_method`
/// and fall back to a fast client-side poll.
fn wait_cmd(args: &[String]) -> Result<i32> {
    let spec = parse_wait(args)?;
    let deadline = spec
        .timeout
        .map(|t| Instant::now() + Duration::from_secs_f64(t));
    match spec.condition {
        WaitFor::Output { needle } => {
            let mut params = json!({ "pane": spec.pane, "match": needle });
            if let Some(timeout) = spec.timeout {
                params["timeout_s"] = json!(timeout);
            }
            let v = send_request("wait.output", params)?;
            let matched = v
                .get("result")
                .and_then(|r| r.get("matched"))
                .and_then(|m| m.as_bool())
                .unwrap_or(false);
            if matched {
                return Ok(0);
            }
            // Pre-`wait.output` server: poll pane.read at a tight interval.
            let unknown = v
                .get("error")
                .and_then(|e| e.get("code"))
                .and_then(|c| c.as_str())
                == Some("invalid_request")
                && v.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .is_some_and(|m| m.starts_with("unknown method"));
            if unknown {
                loop {
                    let v = send_request("pane.read", json!({ "pane": spec.pane }))?;
                    let text = v
                        .get("result")
                        .and_then(|r| r.get("text"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    if text.contains(&needle) {
                        return Ok(0);
                    }
                    if deadline.is_some_and(|d| Instant::now() >= d) {
                        return Ok(2);
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
            // A server error (bad pane id, no active session, invalid timeout) is
            // not a timeout: report it so a failed request is not mistaken for an
            // elapsed deadline.
            if let Some(err) = v.get("error") {
                let msg = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("wait.output failed");
                eprintln!("wait output: {msg}");
            }
            Ok(2)
        }
        WaitFor::AgentStatus { status } => {
            let mut params = json!({"pane":spec.pane, "status":status});
            if let Some(timeout) = spec.timeout {
                params["timeout_s"] = json!(timeout);
            }
            let response = send_request("agent.wait", params)?;
            if response
                .get("result")
                .and_then(|result| result.get("matched"))
                .and_then(Value::as_bool)
                == Some(true)
            {
                return Ok(0);
            }
            let unknown = response
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .is_some_and(|message| message.starts_with("unknown method"));
            if unknown {
                return wait_status_stream(&spec.pane, &status, deadline);
            }
            if let Some(error) = response.get("error") {
                eprintln!(
                    "wait agent-status: {}",
                    error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("agent.wait failed")
                );
            }
            Ok(2)
        }
    }
}

#[derive(Debug, PartialEq)]
enum AgentStartTarget {
    Existing(String),
    Split { anchor: Option<String>, down: bool },
}

fn parse_agent_start_target(args: &[String], caller: Option<String>) -> Result<AgentStartTarget> {
    let pane = flag(args, "--pane");
    let anchor = flag(args, "--anchor");
    if pane.is_some() && anchor.is_some() {
        return Err(anyhow!(
            "agent start accepts either --pane <id> or --anchor <id>, not both"
        ));
    }
    Ok(match pane {
        Some(pane) => AgentStartTarget::Existing(pane),
        None => AgentStartTarget::Split {
            anchor: anchor.or(caller),
            down: args.iter().any(|a| a == "--down"),
        },
    })
}

/// `luvus agent start <name> --kind <kind> [--pane <id> | --anchor <id>] [--down] [--timeout S] [-- <extra…>]` —
/// spawn a coding agent in a sibling pane (or a given one), wait until detection
/// recognizes it, and give it a name, all in one command. Exit 0 when it becomes
/// ready, 2 if it did not within the timeout (the pane and name still exist).
fn agent_start_cmd(args: &[String]) -> Result<i32> {
    let name = args.get(3).cloned().ok_or_else(|| {
        anyhow!("usage: luvus agent start <name> --kind <kind> [--pane <id> | --anchor <id>] [--down] [--timeout S] [-- <extra>]")
    })?;
    let separator = args.iter().position(|arg| arg == "--");
    let options = &args[..separator.unwrap_or(args.len())];
    validate_agent_start_options(options)?;
    let kind =
        flag(options, "--kind").ok_or_else(|| anyhow!("agent start requires --kind <kind>"))?;
    let extra = separator
        .map(|index| args[index + 1..].to_vec())
        .unwrap_or_default();
    let target = parse_agent_start_target(options, std::env::var("LUVUS_PANE_ID").ok())?;
    let mut params = serde_json::Map::new();
    params.insert("name".into(), json!(name));
    params.insert("kind".into(), json!(kind));
    params.insert("args".into(), json!(extra));
    match target {
        AgentStartTarget::Existing(pane) => {
            params.insert("pane".into(), json!(pane));
        }
        AgentStartTarget::Split { anchor, down } => {
            if let Some(anchor) = anchor {
                params.insert("anchor".into(), json!(anchor));
            }
            params.insert(
                "direction".into(),
                json!(if down { "down" } else { "right" }),
            );
        }
    }
    if let Some(timeout) = flag(options, "--timeout") {
        let timeout = timeout
            .parse::<f64>()
            .map_err(|_| anyhow!("--timeout must be seconds between 0 and 3600"))?;
        params.insert("timeout_s".into(), json!(timeout));
    }
    let response = send_request("agent.start", Value::Object(params))?;
    let ready = response["result"]["ready"].as_bool().unwrap_or(false);
    println!(
        "{}",
        serde_json::to_string_pretty(&response).unwrap_or_default()
    );
    Ok(if ready { 0 } else { 2 })
}

fn validate_agent_start_options(args: &[String]) -> Result<()> {
    let mut index = 4;
    while index < args.len() {
        match args[index].as_str() {
            "--kind" | "--pane" | "--anchor" | "--timeout" => {
                if args.get(index + 1).is_none() {
                    return Err(anyhow!("{} requires a value", args[index]));
                }
                index += 2;
            }
            "--down" => index += 1,
            option if option.starts_with("--") => {
                return Err(anyhow!("unknown agent start option: {option}"));
            }
            value => return Err(anyhow!("unexpected agent start argument: {value}")),
        }
    }
    Ok(())
}

/// Manage the one bundled, version-matched Luvus skill. Host-specific paths are
/// reported as installation details, never exposed as separate skills.
fn skill_cmd(rest: &[String]) -> Result<i32> {
    fn no_arguments(rest: &[String]) -> Result<()> {
        if let Some(argument) = rest.get(1) {
            return Err(anyhow!(
                "agent-specific skill management was removed; `luvus skill {}` accepts no arguments (unexpected `{argument}`)",
                rest[0]
            ));
        }
        Ok(())
    }

    match rest.first().map(String::as_str) {
        None | Some("status") => {
            no_arguments(rest)?;
            let statuses = crate::skill::status()?;
            let summary = if statuses.iter().any(|status| {
                matches!(
                    status.state,
                    crate::skill::DestinationState::Modified
                        | crate::skill::DestinationState::Missing
                        | crate::skill::DestinationState::Outdated
                )
            }) {
                "attention"
            } else if statuses.iter().any(|status| {
                matches!(
                    status.state,
                    crate::skill::DestinationState::Current
                        | crate::skill::DestinationState::ExternalCurrent
                        | crate::skill::DestinationState::External
                )
            }) {
                "enabled"
            } else {
                "disabled"
            };
            println!(
                "luvus\t{}\t{summary}",
                crate::skill::bundled_release()
            );
            for status in statuses {
                println!(
                    "{}\t{}\t{}\t{}",
                    status.host,
                    status.state.as_str(),
                    status.managed_release.as_deref().unwrap_or("-"),
                    status.target.display()
                );
            }
            Ok(0)
        }
        Some("enable") => {
            no_arguments(rest)?;
            let mut incomplete = false;
            for change in crate::skill::enable()? {
                incomplete |= change.action == crate::skill::ChangeAction::PreservedModified;
                println!(
                    "{}\t{}\t{}",
                    change.host,
                    change.action.as_str(),
                    change.target.display()
                );
            }
            Ok(if incomplete { 2 } else { 0 })
        }
        Some("disable") => {
            no_arguments(rest)?;
            let mut incomplete = false;
            for change in crate::skill::disable()? {
                incomplete |= change.action == crate::skill::ChangeAction::PreservedModified;
                println!(
                    "{}\t{}\t{}",
                    change.host,
                    change.action.as_str(),
                    change.target.display()
                );
            }
            Ok(if incomplete { 2 } else { 0 })
        }
        Some("show") => {
            no_arguments(rest)?;
            print!("{}", crate::skill::show());
            Ok(0)
        }
        Some("update") => Err(anyhow!(
            "`luvus skill update` was removed; update Luvus, then run `luvus skill enable` to install its version-matched skill"
        )),
        Some("install" | "uninstall" | "on" | "off") => Err(anyhow!(
            "`luvus skill {}` was removed; use `luvus skill {}`",
            rest[0],
            if matches!(rest[0].as_str(), "install" | "on") {
                "enable"
            } else {
                "disable"
            }
        )),
        Some(command) => Err(anyhow!(
            "unknown skill command `{command}`; expected enable, status, disable, or show"
        )),
    }
}

/// `luvus agent prompt <target> <text…> [--wait] [--until STATE] [--timeout S]`
/// submits and waits as one server-owned operation. `agent send` remains a
/// compatibility alias.
fn agent_send_cmd(args: &[String]) -> Result<i32> {
    let target = args.get(3).cloned().ok_or_else(|| {
        anyhow!("usage: luvus agent prompt <target> <text> [--wait] [--until STATE] [--timeout S]")
    })?;
    let mut text_parts = Vec::new();
    let mut wait = false;
    let mut until = Vec::new();
    let mut timeout = None;
    let mut positional_only = false;
    let mut index = 4;
    while index < args.len() {
        let arg = &args[index];
        if positional_only {
            text_parts.push(arg.clone());
            index += 1;
            continue;
        }
        match arg.as_str() {
            "--" => {
                positional_only = true;
                index += 1;
            }
            "--wait" => {
                wait = true;
                index += 1;
            }
            "--until" => {
                until.push(
                    args.get(index + 1)
                        .ok_or_else(|| anyhow!("--until requires a state"))?
                        .clone(),
                );
                index += 2;
            }
            "--timeout" => {
                timeout = Some(
                    args.get(index + 1)
                        .ok_or_else(|| anyhow!("--timeout requires seconds"))?
                        .parse::<f64>()
                        .map_err(|_| anyhow!("--timeout must be seconds between 0 and 3600"))?,
                );
                index += 2;
            }
            option if option.starts_with("--") => {
                return Err(anyhow!("unknown agent prompt option: {option}"));
            }
            _ => {
                text_parts.push(arg.clone());
                index += 1;
            }
        }
    }
    let text = text_parts.join(" ");
    if text.is_empty() {
        return Err(anyhow!("agent prompt requires text"));
    }
    let mut params = serde_json::Map::new();
    params.insert("target".into(), json!(target));
    params.insert("text".into(), json!(text));
    params.insert("wait".into(), json!(wait));
    if !until.is_empty() {
        params.insert("until".into(), json!(until));
    }
    if let Some(timeout) = timeout {
        params.insert("timeout_s".into(), json!(timeout));
    }
    let response = send_request("agent.prompt", Value::Object(params))?;
    let ok = response.get("error").is_none();
    let matched = response["result"]["matched"].as_bool().unwrap_or(!wait);
    println!(
        "{}",
        serde_json::to_string_pretty(&response).unwrap_or_default()
    );
    Ok(if !ok {
        1
    } else if wait && !matched {
        2
    } else {
        0
    })
}

/// Current agent status of `pane` (global lookup via `pane.status`).
fn pane_status(pane: &str) -> Result<Option<String>> {
    let v = send_request("pane.status", json!({ "pane": pane }))?;
    Ok(v.get("result")
        .and_then(|r| r.get("status"))
        .and_then(|x| x.as_str())
        .map(String::from))
}

/// Block until `pane`'s agent reaches `target` (exit 0), or `deadline` passes
/// (exit 2). Subscribes to the event stream **first**, then polls the current
/// status — so a transition that happens between the poll and the subscribe is
/// never missed (it's already buffered on the stream).
fn wait_status_stream(pane: &str, target: &str, deadline: Option<Instant>) -> Result<i32> {
    let path = crate::persist::cli_socket_path();
    let stream =
        crate::ipc::transport::connect(&path).map_err(|_| anyhow!("no luvus server running"))?;
    let mut writer = stream.clone();
    writeln!(
        writer,
        "{}",
        json!({"id":"1","method":"events.subscribe","params":{}})
    )?;
    let mut reader = BufReader::new(stream);

    let (tx, rx) = std::sync::mpsc::channel();
    let (pane_s, target_s) = (pane.to_string(), target.to_string());
    std::thread::spawn(move || {
        while let Ok(Some(l)) = crate::ipc::api::read_stream_frame(&mut reader) {
            if let Ok(v) = serde_json::from_str::<Value>(&l) {
                let is_status =
                    v.get("event").and_then(|e| e.as_str()) == Some("pane.agent_status_changed");
                let data = v.get("data");
                let p = data.and_then(|d| d.get("pane")).and_then(|x| x.as_str());
                let s = data.and_then(|d| d.get("status")).and_then(|x| x.as_str());
                if is_status && p == Some(pane_s.as_str()) && s == Some(target_s.as_str()) {
                    let _ = tx.send(());
                    break;
                }
            }
        }
    });

    // Now that we're listening, an initial poll handles the already-there case.
    if pane_status(pane)?.as_deref() == Some(target) {
        return Ok(0);
    }

    match deadline {
        Some(d) => {
            let now = Instant::now();
            if d <= now {
                return Ok(2);
            }
            match rx.recv_timeout(d - now) {
                Ok(()) => Ok(0),
                Err(_) => Ok(2),
            }
        }
        None => {
            let _ = rx.recv();
            Ok(0)
        }
    }
}

/// Focus + zoom a pane via `attach.pane` (docs/18 WA-2). Used by `luvus attach`.
pub fn request_attach(pane: &str) -> Result<()> {
    send_request("attach.pane", json!({ "pane": pane })).map(|_| ())
}

/// One request/response over the control socket.
pub(crate) fn send_request(method: &str, params: Value) -> Result<Value> {
    let path = crate::persist::cli_socket_path();
    let mut stream =
        crate::ipc::transport::connect(&path).map_err(|_| anyhow!("no luvus server running"))?;
    let req = json!({ "id": "1", "method": method, "params": params });
    writeln!(stream, "{req}")?;
    let mut reader = BufReader::new(stream);
    let line = crate::ipc::api::read_response_frame(&mut reader)?;
    serde_json::from_str(&line).map_err(|e| anyhow!("bad reply: {e}"))
}

/// Transport-neutral one-frame bridge for harnesses that cannot use Unix
/// sockets or Windows named pipes directly. This is intentionally not a shell
/// wrapper: it forwards one bounded protocol frame to the selected session and
/// writes one bounded response. It composes over SSH as
/// `ssh host luvus uhp proxy` without opening a network listener.
fn uhp_proxy() -> Result<i32> {
    let mut input = std::io::BufReader::new(std::io::stdin().lock());
    let request = crate::ipc::api::read_request_frame(&mut input)
        .map_err(|error| anyhow!("invalid request frame: {error}"))?;
    let path = crate::persist::cli_socket_path();
    let mut stream = crate::ipc::transport::connect(&path)
        .map_err(|_| anyhow!("no luvus server running (socket: {})", path.display()))?;
    writeln!(stream, "{request}")?;
    let mut reader = BufReader::new(stream);
    let response = crate::ipc::api::read_response_frame(&mut reader)?;
    validate_response_id(&request, &response)?;
    println!("{response}");
    Ok(api_response_exit_code(&response))
}

fn validate_response_id(request: &str, response: &str) -> Result<()> {
    let request: Value = serde_json::from_str(request)
        .map_err(|error| anyhow!("invalid request envelope: {error}"))?;
    let response: Value = serde_json::from_str(response)
        .map_err(|error| anyhow!("invalid response envelope: {error}"))?;
    let request_id = request
        .get("id")
        .ok_or_else(|| anyhow!("request envelope is missing id"))?;
    let response_id = response
        .get("id")
        .ok_or_else(|| anyhow!("response envelope is missing id"))?;
    if response_id != request_id {
        return Err(anyhow!(
            "response id does not match request id: expected {request_id}, received {response_id}"
        ));
    }
    Ok(())
}

fn api_response_exit_code(response: &str) -> i32 {
    if serde_json::from_str::<Value>(response)
        .ok()
        .is_some_and(|value| value.get("error").is_some())
    {
        1
    } else {
        0
    }
}

/// Register an installed module by writing the registry file directly (used when
/// no server is running).
fn register_directly(installed: &crate::module::install::Installed) -> Result<()> {
    use crate::module::{manifest::ModuleManifest, registry, InstalledModule};
    let mut reg = registry::load();
    reg.modules.retain(|m| m.id != installed.id);
    let manifest = ModuleManifest::load(&installed.root).map_err(|e| anyhow!(e))?;
    reg.modules.push(InstalledModule {
        id: installed.id.clone(),
        root: installed.root.clone(),
        enabled: true,
        source: Some(installed.source.clone()),
        manifest,
        warning: None,
    });
    registry::save(&reg);
    Ok(())
}

/// Map argv to a `(method, params)` pair.
fn parse(args: &[String]) -> Result<(String, Value)> {
    let noun = args.get(1).map(String::as_str).unwrap_or("");
    let verb = args.get(2).map(String::as_str).unwrap_or("");
    let rest = &args[3.min(args.len())..];
    if noun == "uhp" {
        if !rest.is_empty() {
            return Err(anyhow!(
                "usage: luvus uhp <capabilities|schema|snapshot|events|proxy>"
            ));
        }
        return match verb {
            "capabilities" => Ok(("uhp.capabilities".into(), json!({}))),
            "snapshot" => Ok(("session.snapshot".into(), json!({}))),
            "events" => Ok(("events.subscribe".into(), json!({}))),
            _ => Err(anyhow!(
                "usage: luvus uhp <capabilities|schema|snapshot|events|proxy>"
            )),
        };
    }

    // The pane id is the first numeric positional, else $LUVUS_PANE_ID.
    let pane = || -> Value {
        if let Some(first) = rest.first() {
            if first.parse::<u32>().is_ok() {
                return json!(first);
            }
        }
        match std::env::var("LUVUS_PANE_ID") {
            Ok(v) => json!(v),
            Err(_) => Value::Null,
        }
    };
    // Args after an optional leading numeric pane id.
    let tail = || -> Vec<String> {
        let skip = rest
            .first()
            .map(|s| s.parse::<u32>().is_ok())
            .unwrap_or(false);
        rest[if skip { 1 } else { 0 }..].to_vec()
    };

    let with_pane = |mut obj: serde_json::Map<String, Value>| {
        let p = pane();
        if !p.is_null() {
            obj.insert("pane".to_string(), p);
        }
        Value::Object(obj)
    };

    // First positional arg after the verb (for workspace/tab indices).
    let arg0 = || rest.first().cloned();
    // Everything after the verb, joined, minus flags and their values — for free
    // text like a tab name, where `luvus tab rename my tab --tab 2` must not
    // fold the flag into the name.
    let rest_text = || {
        let mut out: Vec<&str> = Vec::new();
        let mut skip = false;
        for a in rest {
            if skip {
                skip = false;
                continue;
            }
            if a.starts_with("--") {
                skip = true; // assume `--flag value`; a bare flag just eats the next word
                continue;
            }
            out.push(a);
        }
        out.join(" ")
    };
    let one = |key: &str, val: Option<String>| {
        let mut obj = serde_json::Map::new();
        if let Some(v) = val {
            obj.insert(key.to_string(), json!(v));
        }
        Value::Object(obj)
    };

    Ok(match (noun, verb) {
        ("ping", _) => ("ping".into(), json!({})),
        ("events", _) => ("events.subscribe".into(), json!({})),
        // Exact scrollback search remains the default for script compatibility.
        // The universal finder is deliberately opt-in through `--fuzzy`.
        ("search", _) => {
            let search_args = &args[2.min(args.len())..];
            let fuzzy = search_args
                .iter()
                .take_while(|arg| arg.as_str() != "--")
                .any(|arg| arg == "--fuzzy");
            let mut query: Vec<String> = Vec::new();
            let mut case_sensitive = false;
            let mut scope = "all".to_string();
            let mut all_sessions = false;
            let mut limit = crate::search::RESULT_CAP as u64;
            let mut index = 0;
            while index < search_args.len() {
                match search_args[index].as_str() {
                    "--" => {
                        query.extend_from_slice(&search_args[index + 1..]);
                        break;
                    }
                    "--fuzzy" | "--json" => index += 1,
                    "--case" | "--case-sensitive" => {
                        case_sensitive = true;
                        index += 1;
                    }
                    "--all-sessions" if fuzzy => {
                        all_sessions = true;
                        index += 1;
                    }
                    "--scope" if fuzzy => {
                        let value = search_args
                            .get(index + 1)
                            .ok_or_else(|| anyhow!("--scope requires a value"))?;
                        if !matches!(value.as_str(), "all" | "navigate" | "files" | "output") {
                            return Err(anyhow!("--scope must be all, navigate, files, or output"));
                        }
                        scope = value.clone();
                        index += 2;
                    }
                    "--limit" if fuzzy => {
                        let value = search_args
                            .get(index + 1)
                            .ok_or_else(|| anyhow!("--limit requires a value"))?;
                        limit = value
                            .parse::<u64>()
                            .ok()
                            .filter(|value| (1..=crate::search::RESULT_CAP as u64).contains(value))
                            .ok_or_else(|| anyhow!("--limit must be between 1 and 200"))?;
                        index += 2;
                    }
                    flag if flag.starts_with("--") => {
                        return Err(anyhow!("unknown search option: {flag}"));
                    }
                    value => {
                        query.push(value.to_string());
                        index += 1;
                    }
                }
            }
            if query.is_empty() {
                return Err(anyhow!(if fuzzy {
                    "usage: luvus search --fuzzy <query...> [--scope all|navigate|files|output] [--all-sessions] [--limit 1-200] [--case] [--json]"
                } else {
                    "usage: luvus search <text...> [--case]"
                }));
            }
            if fuzzy {
                (
                    "search.query".into(),
                    json!({
                        "query": query.join(" "),
                        "scope": scope,
                        "all_sessions": all_sessions,
                        "limit": limit,
                        "case_sensitive": case_sensitive,
                    }),
                )
            } else {
                (
                    "search".into(),
                    json!({ "query": query.join(" "), "case_sensitive": case_sensitive }),
                )
            }
        }
        ("agent", "sessions") => ("agent.sessions".into(), json!({})),
        ("agent", "resume") => ("agent.resume".into(), one("session_id", arg0())),
        ("agent", "fork") => {
            let usage = "usage: luvus agent fork <target> [--name <alias>] [--no-focus]";
            let target = rest
                .first()
                .filter(|v| !v.starts_with("--"))
                .ok_or_else(|| anyhow!(usage))?
                .clone();
            let mut name: Option<String> = None;
            let mut no_focus = false;
            let mut i = 1;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--name" => {
                        let alias = rest
                            .get(i + 1)
                            .filter(|v| !v.starts_with("--"))
                            .ok_or_else(|| anyhow!("--name needs an agent alias"))?;
                        if name.is_some() {
                            return Err(anyhow!("--name may be passed only once"));
                        }
                        name = Some(alias.clone());
                        i += 2;
                    }
                    "--no-focus" => {
                        if no_focus {
                            return Err(anyhow!("--no-focus may be passed only once"));
                        }
                        no_focus = true;
                        i += 1;
                    }
                    option if option.starts_with("--") => {
                        return Err(anyhow!("unknown agent fork option `{option}`. {usage}"));
                    }
                    extra => {
                        return Err(anyhow!("unexpected agent fork argument `{extra}`. {usage}"));
                    }
                }
            }
            let mut obj = serde_json::Map::new();
            obj.insert("target".to_string(), json!(target));
            if let Some(alias) = name {
                obj.insert("name".to_string(), json!(alias));
            }
            if no_focus {
                obj.insert("focus".to_string(), json!(false));
            }
            ("agent.fork".into(), Value::Object(obj))
        }
        ("agent", "name") => {
            // `agent name <name>` names the current pane; `--pane <id>` overrides;
            // `--clear` drops the pane's alias. The name never doubles as a pane id.
            let mut obj = serde_json::Map::new();
            if let Some(pid) = flag(args, "--pane").or_else(|| std::env::var("LUVUS_PANE_ID").ok())
            {
                obj.insert("pane".to_string(), json!(pid));
            }
            if args.iter().any(|a| a == "--clear") {
                obj.insert("clear".to_string(), json!(true));
            } else if let Some(name) = rest.first() {
                obj.insert("name".to_string(), json!(name));
            }
            ("agent.name".into(), Value::Object(obj))
        }
        ("agent", "keys") => {
            // `agent keys <target> <key> [key ...]`
            let mut obj = serde_json::Map::new();
            if let Some(t) = rest.first() {
                obj.insert("target".to_string(), json!(t));
            }
            let keys: Vec<String> = rest.iter().skip(1).cloned().collect();
            obj.insert("keys".to_string(), json!(keys));
            ("agent.keys".into(), Value::Object(obj))
        }
        ("agent", "get") => {
            // `agent get <target>` — one agent's live info
            ("agent.get".into(), one("target", rest.first().cloned()))
        }
        ("agent", "explain") => ("agent.explain".into(), one("target", rest.first().cloned())),
        ("agent", "report") => {
            let usage = "usage: luvus agent report [<pane>] --source <id> --kind <agent> --status idle|working|blocked|done [--message <text>] [--session <id>] [--sequence N] [--ttl S]";
            let mut obj = serde_json::Map::new();
            let pane_id = rest
                .first()
                .filter(|value| value.parse::<u32>().is_ok())
                .cloned()
                .or_else(|| std::env::var("LUVUS_PANE_ID").ok())
                .ok_or_else(|| anyhow!(usage))?;
            obj.insert("pane".into(), json!(pane_id));
            obj.insert(
                "source".into(),
                json!(flag(args, "--source").ok_or_else(|| anyhow!(usage))?),
            );
            obj.insert(
                "agent".into(),
                json!(flag(args, "--kind").ok_or_else(|| anyhow!(usage))?),
            );
            obj.insert(
                "status".into(),
                json!(flag(args, "--status").ok_or_else(|| anyhow!(usage))?),
            );
            if let Some(value) = flag(args, "--message") {
                obj.insert("message".into(), json!(value));
            }
            if let Some(value) = flag(args, "--session") {
                obj.insert("session_id".into(), json!(value));
            }
            if let Some(value) = flag(args, "--sequence") {
                obj.insert(
                    "sequence".into(),
                    json!(value
                        .parse::<u64>()
                        .map_err(|_| anyhow!("--sequence must be a non-negative integer"))?),
                );
            }
            if let Some(value) = flag(args, "--ttl") {
                obj.insert(
                    "ttl_s".into(),
                    json!(value
                        .parse::<u64>()
                        .map_err(|_| anyhow!("--ttl must be whole seconds"))?),
                );
            }
            ("agent.report".into(), Value::Object(obj))
        }
        ("agent", "release") => {
            let usage = "usage: luvus agent release [<pane>] --source <id>";
            let mut obj = serde_json::Map::new();
            let pane_id = rest
                .first()
                .filter(|value| value.parse::<u32>().is_ok())
                .cloned()
                .or_else(|| std::env::var("LUVUS_PANE_ID").ok())
                .ok_or_else(|| anyhow!(usage))?;
            obj.insert("pane".into(), json!(pane_id));
            obj.insert(
                "source".into(),
                json!(flag(args, "--source").ok_or_else(|| anyhow!(usage))?),
            );
            ("agent.release".into(), Value::Object(obj))
        }
        ("agent", "read") => {
            // `agent read <target> [--lines N] [--source visible|recent]`
            let mut obj = serde_json::Map::new();
            if let Some(t) = rest.first() {
                obj.insert("target".to_string(), json!(t));
            }
            if let Some(n) = flag(args, "--lines").and_then(|s| s.parse::<u64>().ok()) {
                obj.insert("lines".to_string(), json!(n));
            }
            if let Some(s) = flag(args, "--source") {
                obj.insert("source".to_string(), json!(s));
            }
            ("agent.read".into(), Value::Object(obj))
        }
        ("agent", _) => ("agent.list".into(), json!({})),

        ("ui", "sidebar") => {
            let mut obj = serde_json::Map::new();
            if let Some(s) = flag(args, "--side") {
                obj.insert("side".to_string(), json!(s));
            }
            if let Some(w) = flag(args, "--width") {
                obj.insert("width".to_string(), json!(w));
            }
            if args.iter().any(|a| a == "--hide") {
                obj.insert("visible".to_string(), json!(false));
            } else if args.iter().any(|a| a == "--show") {
                obj.insert("visible".to_string(), json!(true));
            }
            ("ui.sidebar".into(), Value::Object(obj))
        }

        // Sidebar docks for module/plugin authors (docs/29). `push` feeds rows to
        // a dock: `--rows '<json array>'`, or pipe the JSON array on stdin.
        ("ui", "toast") => ("ui.toast".into(), one("text", Some(rest_text()))),
        ("ui", "dock") => {
            let sub = rest.first().map(String::as_str).unwrap_or("list");
            match sub {
                "push" => {
                    let mut obj = serde_json::Map::new();
                    let id = flag(args, "--id")
                        .ok_or_else(|| anyhow!("usage: luvus ui dock push --id <id> [--title <t>] [--side left|right] [--rows <json>]"))?;
                    obj.insert("id".to_string(), json!(id));
                    if let Some(tt) = flag(args, "--title") {
                        obj.insert("title".to_string(), json!(tt));
                    }
                    if let Some(side) = flag(args, "--side") {
                        obj.insert("placement".to_string(), json!(side));
                    }
                    let rows_str = match flag(args, "--rows") {
                        Some(s) => s,
                        None => {
                            use std::io::Read;
                            let mut s = String::new();
                            let _ = std::io::stdin().read_to_string(&mut s);
                            s
                        }
                    };
                    let rows: Value = if rows_str.trim().is_empty() {
                        json!([])
                    } else {
                        serde_json::from_str(&rows_str)
                            .map_err(|e| anyhow!("--rows must be a JSON array: {e}"))?
                    };
                    obj.insert("rows".to_string(), rows);
                    ("ui.dock.push".into(), Value::Object(obj))
                }
                "move" => {
                    let mut obj = serde_json::Map::new();
                    if let Some(id) = flag(args, "--id") {
                        obj.insert("id".to_string(), json!(id));
                    }
                    if let Some(side) = flag(args, "--side") {
                        obj.insert("side".to_string(), json!(side));
                    }
                    ("ui.dock.move".into(), Value::Object(obj))
                }
                _ => ("ui.dock.list".into(), json!({})),
            }
        }
        ("bar", sub) => {
            let mut obj = serde_json::Map::new();
            if let Ok(owner) = std::env::var("LUVUS_MODULE_ID") {
                obj.insert("owner".into(), json!(owner));
            }
            match sub {
                "push" => {
                    let usage = "usage: luvus bar push --id <id> [--region top-right|bottom-right] (--content <json>|--content-file <path>|--text <text>|--state <state>)";
                    let id = flag(args, "--id").ok_or_else(|| anyhow!(usage))?;
                    obj.insert("id".into(), json!(id));
                    if let Some(region) = flag(args, "--region") {
                        if !matches!(
                            region.as_str(),
                            "top" | "top-right" | "bottom" | "bottom-right"
                        ) {
                            return Err(anyhow!("--region must be top-right or bottom-right"));
                        }
                        obj.insert("region".into(), json!(region));
                    }
                    if let Some(priority) = flag(args, "--priority") {
                        let priority = priority
                            .parse::<u8>()
                            .map_err(|_| anyhow!("--priority must be 0..255"))?;
                        obj.insert("priority".into(), json!(priority));
                    }
                    let inline = flag(args, "--content");
                    let file = flag(args, "--content-file");
                    if inline.is_some() && file.is_some() {
                        return Err(anyhow!("use only one of --content and --content-file"));
                    }
                    let content = if let Some(raw) = inline {
                        serde_json::from_str(&raw).map_err(|error| {
                            anyhow!("--content must be a JSON segment array: {error}")
                        })?
                    } else if let Some(path) = file {
                        let raw = std::fs::read_to_string(&path)
                            .map_err(|error| anyhow!("cannot read {path}: {error}"))?;
                        serde_json::from_str(&raw).map_err(|error| {
                            anyhow!("{path} must contain a JSON segment array: {error}")
                        })?
                    } else {
                        let mut segments = Vec::new();
                        if let Some(text) = flag(args, "--text") {
                            segments.push(json!({"type":"text","text":text}));
                        }
                        if let Some(state) = flag(args, "--state") {
                            segments.push(json!({"type":"state","state":state}));
                        }
                        if segments.is_empty() {
                            return Err(anyhow!(usage));
                        }
                        if let Some(last) = segments.last_mut().and_then(Value::as_object_mut) {
                            if let Some(action) = flag(args, "--action") {
                                last.insert("action".into(), json!(action));
                            }
                            if let Some(value) = flag(args, "--value") {
                                last.insert("value".into(), json!(value));
                            }
                        }
                        Value::Array(segments)
                    };
                    if !content.is_array() {
                        return Err(anyhow!("bar content must be a JSON segment array"));
                    }
                    obj.insert("content".into(), content);
                    if let Some(raw) = flag(args, "--compact-content") {
                        let compact: Value = serde_json::from_str(&raw).map_err(|error| {
                            anyhow!("--compact-content must be a JSON segment array: {error}")
                        })?;
                        if !compact.is_array() {
                            return Err(anyhow!(
                                "compact bar content must be a JSON segment array"
                            ));
                        }
                        obj.insert("compact_content".into(), compact);
                    }
                    ("ui.bar.push".into(), Value::Object(obj))
                }
                "move" => {
                    let id = flag(args, "--id").ok_or_else(|| {
                        anyhow!(
                            "usage: luvus bar move --id <id> --region top-right|bottom-right|off"
                        )
                    })?;
                    let region =
                        flag(args, "--region").ok_or_else(|| anyhow!("--region is required"))?;
                    if !matches!(
                        region.as_str(),
                        "top" | "top-right" | "bottom" | "bottom-right" | "off"
                    ) {
                        return Err(anyhow!("--region must be top-right, bottom-right, or off"));
                    }
                    obj.insert("id".into(), json!(id));
                    obj.insert("region".into(), json!(region));
                    ("ui.bar.move".into(), Value::Object(obj))
                }
                "remove" => {
                    let id = flag(args, "--id")
                        .ok_or_else(|| anyhow!("usage: luvus bar remove --id <id>"))?;
                    obj.insert("id".into(), json!(id));
                    ("ui.bar.remove".into(), Value::Object(obj))
                }
                "" | "list" => ("ui.bar.list".into(), Value::Object(obj)),
                _ => return Err(anyhow!("usage: luvus bar list|push|move|remove")),
            }
        }
        ("ui", "notification") => {
            let sub = rest.first().map(String::as_str).unwrap_or("push");
            let mut obj = serde_json::Map::new();
            if let Ok(owner) = std::env::var("LUVUS_MODULE_ID") {
                obj.insert("owner".into(), json!(owner));
            }
            match sub {
                "push" => {
                    let text = flag(args, "--text").ok_or_else(|| anyhow!("usage: luvus ui notification push --text <text> [--level info|success|warning|error] [--ttl-ms <n>]"))?;
                    obj.insert("text".into(), json!(text));
                    if let Some(level) = flag(args, "--level") {
                        if !matches!(level.as_str(), "info" | "success" | "warning" | "error") {
                            return Err(anyhow!("unknown notification level {level}"));
                        }
                        obj.insert("level".into(), json!(level));
                    }
                    if let Some(ttl) = flag(args, "--ttl-ms") {
                        obj.insert(
                            "ttl_ms".into(),
                            json!(ttl
                                .parse::<u64>()
                                .map_err(|_| anyhow!("--ttl-ms must be a positive integer"))?),
                        );
                    }
                    for (flag_name, key) in [
                        ("--action", "action"),
                        ("--value", "value"),
                        ("--dedupe-key", "dedupe_key"),
                    ] {
                        if let Some(value) = flag(args, flag_name) {
                            obj.insert(key.into(), json!(value));
                        }
                    }
                    ("ui.notification.push".into(), Value::Object(obj))
                }
                "clear" => {
                    if let Some(key) = flag(args, "--dedupe-key") {
                        obj.insert("dedupe_key".into(), json!(key));
                    }
                    ("ui.notification.clear".into(), Value::Object(obj))
                }
                _ => return Err(anyhow!("usage: luvus ui notification push|clear")),
            }
        }

        ("workspace" | "node", "new") => ("workspace.new".into(), json!({})),
        ("workspace" | "node", "open") => ("workspace.open".into(), one("path", arg0())),
        ("workspace" | "node", "focus") => ("workspace.focus".into(), one("workspace", arg0())),
        ("workspace" | "node", "rename") => {
            let usage = "usage: luvus workspace rename <i> <name>";
            if rest.len() < 2 {
                return Err(anyhow!(usage));
            }
            let workspace = rest[0]
                .parse::<usize>()
                .map_err(|_| anyhow!("workspace must be a 0-based number. {usage}"))?;
            let name = rest[1..].join(" ");
            let name = name.trim();
            if name.is_empty() {
                return Err(anyhow!("workspace name must not be empty"));
            }
            if name.chars().count() > crate::app::WS_NAME_MAX {
                return Err(anyhow!(
                    "workspace name must be at most {} characters",
                    crate::app::WS_NAME_MAX
                ));
            }
            (
                "workspace.rename".into(),
                json!({"workspace": workspace.to_string(), "name": name}),
            )
        }
        ("workspace" | "node", action @ ("pin" | "unpin")) => {
            let usage = format!("usage: luvus workspace {action} <i>");
            if rest.len() != 1 {
                return Err(anyhow!(usage));
            }
            let workspace = rest[0]
                .parse::<usize>()
                .map_err(|_| anyhow!("workspace must be a 0-based number. {usage}"))?;
            (
                "workspace.pin".into(),
                json!({"workspace": workspace.to_string(), "pinned": action == "pin"}),
            )
        }
        ("workspace" | "node", "close") => ("workspace.close".into(), one("workspace", arg0())),
        ("workspace" | "node", "" | "list") => ("workspace.list".into(), json!({})),
        ("workspace" | "node", other) => {
            return Err(anyhow!(
                "unknown workspace command `{other}`. Try `luvus help workspace`."
            ))
        }

        ("tab", "new") => ("tab.new".into(), json!({})),
        ("tab", "focus") => {
            if rest.len() != 1 {
                return Err(anyhow!("usage: luvus tab focus <n>"));
            }
            let tab = rest[0]
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| anyhow!("tab position must be a positive 1-based number"))?;
            ("tab.focus".into(), json!({"tab": tab.to_string()}))
        }
        ("tab", "move") => {
            let parse_position = |raw: &str| -> Result<String> {
                raw.parse::<usize>()
                    .ok()
                    .filter(|n| *n > 0)
                    .map(|n| n.to_string())
                    .ok_or_else(|| anyhow!("tab positions must be positive 1-based numbers"))
            };
            let usage = "usage: luvus tab move <from> <to> | left|right [--tab N]";
            if rest
                .first()
                .is_some_and(|arg| matches!(arg.as_str(), "left" | "right"))
            {
                let valid_shape = rest.len() == 1 || (rest.len() == 3 && rest[1] == "--tab");
                if !valid_shape {
                    return Err(anyhow!(usage));
                }
                let mut params = serde_json::Map::new();
                params.insert("direction".to_string(), json!(rest[0]));
                if rest.len() == 3 {
                    params.insert("tab".to_string(), json!(parse_position(&rest[2])?));
                }
                ("tab.move".into(), Value::Object(params))
            } else {
                if rest.len() != 2 {
                    return Err(anyhow!(usage));
                }
                let from = parse_position(&rest[0])?;
                let to = parse_position(&rest[1])?;
                ("tab.move".into(), json!({"tab": from, "to": to}))
            }
        }
        ("tab", "swap") => {
            let usage = "usage: luvus tab swap <first> <second>";
            if rest.len() != 2 {
                return Err(anyhow!(usage));
            }
            let parse_position = |raw: &str| -> Result<String> {
                raw.parse::<usize>()
                    .ok()
                    .filter(|n| *n > 0)
                    .map(|n| n.to_string())
                    .ok_or_else(|| anyhow!("tab positions must be positive 1-based numbers"))
            };
            let first = parse_position(&rest[0])?;
            let second = parse_position(&rest[1])?;
            if first == second {
                return Err(anyhow!("tab positions must differ"));
            }
            ("tab.swap".into(), json!({"tab": first, "with": second}))
        }
        ("tab", "close") => ("tab.close".into(), one("tab", arg0())),
        // `tab rename <name>` names the active tab; `--tab N` targets another.
        ("tab", "rename") => {
            let mut words = Vec::new();
            let mut tab = None;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--tab" => {
                        if tab.is_some() || i + 1 >= rest.len() {
                            return Err(anyhow!("usage: luvus tab rename <name> [--tab N]"));
                        }
                        let position = rest[i + 1]
                            .parse::<usize>()
                            .ok()
                            .filter(|n| *n > 0)
                            .ok_or_else(|| {
                                anyhow!("tab position must be a positive 1-based number")
                            })?;
                        tab = Some(position.to_string());
                        i += 2;
                    }
                    arg if arg.starts_with("--") => {
                        return Err(anyhow!("unknown tab rename option `{arg}`"));
                    }
                    _ => {
                        words.push(rest[i].as_str());
                        i += 1;
                    }
                }
            }
            let name = words.join(" ");
            if name.chars().count() > crate::app::TAB_NAME_MAX {
                return Err(anyhow!(
                    "tab name must be at most {} characters",
                    crate::app::TAB_NAME_MAX
                ));
            }
            let mut obj = serde_json::Map::new();
            obj.insert("name".to_string(), json!(name));
            if let Some(n) = tab {
                obj.insert("tab".to_string(), json!(n));
            }
            ("tab.rename".into(), Value::Object(obj))
        }
        ("tab", "" | "list") => ("tab.list".into(), json!({})),
        ("tab", other) => {
            return Err(anyhow!(
                "unknown tab command `{other}`. Try `luvus help tab`."
            ))
        }

        ("pane", "split") => {
            let mut obj = serde_json::Map::new();
            if args.iter().any(|a| a == "--down" || a == "--stack") {
                obj.insert("direction".to_string(), json!("down"));
            }
            if args.iter().any(|a| a == "--no-focus") {
                obj.insert("focus".to_string(), json!(false));
            }
            ("pane.split".into(), with_pane(obj))
        }
        ("pane", "focus") => ("pane.focus".into(), with_pane(serde_json::Map::new())),
        ("pane", "move") => {
            let usage = "usage: luvus pane move [<id>] (--tab <n> | --new-tab)";
            let mut pane_id: Option<String> = None;
            let mut tab: Option<String> = None;
            let mut new_tab = false;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--tab" => {
                        let raw = rest
                            .get(i + 1)
                            .filter(|v| !v.starts_with("--"))
                            .ok_or_else(|| anyhow!("--tab needs a positive 1-based number"))?;
                        if tab.is_some() {
                            return Err(anyhow!("--tab may be passed only once"));
                        }
                        tab = Some(raw.clone());
                        i += 2;
                    }
                    "--new-tab" => {
                        if new_tab {
                            return Err(anyhow!("--new-tab may be passed only once"));
                        }
                        new_tab = true;
                        i += 1;
                    }
                    arg if arg.starts_with("--") => {
                        return Err(anyhow!("unknown pane move option `{arg}`. {usage}"));
                    }
                    raw => {
                        if pane_id.is_some() || raw.parse::<u32>().is_err() {
                            return Err(anyhow!("pane id must be one numeric value. {usage}"));
                        }
                        pane_id = Some(raw.to_string());
                        i += 1;
                    }
                }
            }
            if new_tab == tab.is_some() {
                return Err(anyhow!(
                    "pass exactly one destination: --tab <n> or --new-tab"
                ));
            }
            let mut obj = serde_json::Map::new();
            if new_tab {
                obj.insert("new_tab".to_string(), json!(true));
            } else {
                let raw = tab.unwrap();
                let n = raw
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| anyhow!("--tab must be a positive 1-based number"))?;
                obj.insert("tab".to_string(), json!(n.to_string()));
            }
            if let Some(pane) = pane_id.or_else(|| std::env::var("LUVUS_PANE_ID").ok()) {
                obj.insert("pane".to_string(), json!(pane));
            }
            ("pane.move".into(), Value::Object(obj))
        }
        ("pane", "run") => {
            let command = tail().join(" ");
            let mut obj = serde_json::Map::new();
            obj.insert("command".to_string(), json!(command));
            ("pane.run".into(), with_pane(obj))
        }
        ("pane", "send") => {
            let text = tail().join(" ");
            let mut obj = serde_json::Map::new();
            obj.insert("text".to_string(), json!(text));
            ("pane.send_input".into(), with_pane(obj))
        }
        ("pane", "read") => ("pane.read".into(), with_pane(serde_json::Map::new())),
        ("pane", "status") => ("pane.status".into(), with_pane(serde_json::Map::new())),
        ("pane", "processes") => ("pane.processes".into(), with_pane(serde_json::Map::new())),
        ("pane", "close") => ("pane.close".into(), with_pane(serde_json::Map::new())),
        // `pane name <name>` is a synonym for `agent name`: it aliases the pane so
        // you can mention it by name. The name never doubles as a pane id.
        ("pane", "name") => {
            let mut obj = serde_json::Map::new();
            if let Some(pid) = flag(args, "--pane").or_else(|| std::env::var("LUVUS_PANE_ID").ok())
            {
                obj.insert("pane".to_string(), json!(pid));
            }
            if args.iter().any(|a| a == "--clear") {
                obj.insert("clear".to_string(), json!(true));
            } else if let Some(name) = rest.first() {
                obj.insert("name".to_string(), json!(name));
            }
            ("agent.name".into(), Value::Object(obj))
        }
        ("pane", "report") => {
            let mut obj = serde_json::Map::new();
            obj.insert(
                "agent".to_string(),
                json!(flag(args, "--agent").unwrap_or_default()),
            );
            obj.insert(
                "session_id".to_string(),
                json!(flag(args, "--session").unwrap_or_default()),
            );
            ("pane.report_session".into(), with_pane(obj))
        }
        ("pane", "report-event") => {
            let mut obj = serde_json::Map::new();
            obj.insert(
                "agent".to_string(),
                json!(flag(args, "--agent").unwrap_or_default()),
            );
            obj.insert(
                "kind".to_string(),
                json!(flag(args, "--kind").unwrap_or_default()),
            );
            obj.insert(
                "message".to_string(),
                json!(flag(args, "--message").unwrap_or_default()),
            );
            if let Some(t) = flag(args, "--tool") {
                obj.insert("tool".to_string(), json!(t));
            }
            ("pane.report_event".into(), with_pane(obj))
        }
        ("pane", "" | "list") => ("pane.list".into(), json!({})),
        ("pane", other) => {
            return Err(anyhow!(
                "unknown pane command `{other}`. Try `luvus help pane`."
            ))
        }

        ("module", "link") => {
            let mut obj = serde_json::Map::new();
            if let Some(path) = rest.first() {
                obj.insert("path".to_string(), json!(path));
            }
            if args.iter().any(|a| a == "--disabled") {
                obj.insert("disabled".to_string(), json!(true));
            }
            ("module.link".into(), Value::Object(obj))
        }
        ("module", "unlink") => ("module.unlink".into(), one("id", arg0())),
        ("module", "uninstall") => ("module.uninstall".into(), one("id", arg0())),
        ("module", "enable") => ("module.enable".into(), one("id", arg0())),
        ("module", "disable") => ("module.disable".into(), one("id", arg0())),
        ("module", "run") => {
            let mut obj = serde_json::Map::new();
            match (rest.first(), rest.get(1)) {
                (Some(m), Some(a)) => {
                    obj.insert("module".to_string(), json!(m));
                    obj.insert("id".to_string(), json!(a));
                }
                (Some(a), None) => {
                    obj.insert("id".to_string(), json!(a));
                }
                _ => return Err(anyhow!("usage: luvus module run <module> <action>")),
            }
            ("module.action.invoke".into(), Value::Object(obj))
        }
        ("module", "actions") => ("module.action.list".into(), json!({})),
        ("module", "log") => {
            let mut obj = serde_json::Map::new();
            if let Some(id) = rest.first().filter(|s| !s.starts_with("--")) {
                obj.insert("id".to_string(), json!(id));
            }
            if let Some(n) = flag(args, "--limit").and_then(|s| s.parse::<u64>().ok()) {
                obj.insert("limit".to_string(), json!(n));
            }
            ("module.log.list".into(), Value::Object(obj))
        }
        ("module", "info") => ("module.info".into(), one("id", arg0())),
        ("module", "config-dir") => ("module.config_dir".into(), one("id", arg0())),
        // `module settings <id>` lists; `… <id> <key>` reads; `… <id> <key> <value>` writes.
        ("module", "settings") => {
            let mut obj = serde_json::Map::new();
            let Some(id) = rest.first() else {
                return Err(anyhow!(
                    "usage: luvus module settings <id> [<key> [<value>]]"
                ));
            };
            obj.insert("id".to_string(), json!(id));
            match (rest.get(1), rest.get(2)) {
                (Some(k), Some(v)) => {
                    obj.insert("key".to_string(), json!(k));
                    // A bare word is sent as-is; the server coerces it to the
                    // declared type, so `--json` is only needed for arrays.
                    obj.insert("value".to_string(), parse_setting_value(v));
                    ("module.settings.set".into(), Value::Object(obj))
                }
                (Some(k), None) => {
                    obj.insert("key".to_string(), json!(k));
                    ("module.settings.get".into(), Value::Object(obj))
                }
                _ => ("module.settings.list".into(), Value::Object(obj)),
            }
        }
        ("module", "pane") => {
            let sub = rest.first().map(String::as_str).unwrap_or("");
            match sub {
                "open" => {
                    let mut obj = serde_json::Map::new();
                    if let Some(m) = rest.get(1) {
                        obj.insert("module".to_string(), json!(m));
                    }
                    if let Some(e) = rest.get(2) {
                        obj.insert("entrypoint".to_string(), json!(e));
                    }
                    if let Some(pl) = flag(args, "--placement") {
                        obj.insert("placement".to_string(), json!(pl));
                    }
                    ("module.pane.open".into(), Value::Object(obj))
                }
                "focus" => (
                    "module.pane.focus".into(),
                    one("pane", rest.get(1).cloned()),
                ),
                "close" => (
                    "module.pane.close".into(),
                    one("pane", rest.get(1).cloned()),
                ),
                _ => return Err(anyhow!("usage: luvus module pane open|focus|close …")),
            }
        }
        ("module", _) => ("module.list".into(), json!({})),

        ("diff", "refresh") => {
            if !rest.is_empty() {
                return Err(anyhow!("usage: luvus diff refresh"));
            }
            ("diff.refresh".into(), json!({}))
        }
        ("diff", "" | "list") => {
            let mut obj = serde_json::Map::new();
            if let Some(layer) = flag(args, "--layer") {
                obj.insert("layer".into(), json!(layer));
            }
            ("diff.list".into(), Value::Object(obj))
        }
        ("diff", "open") => {
            let mut obj = serde_json::Map::new();
            if let Some(path) = rest.first().filter(|value| !value.starts_with("--")) {
                obj.insert("path".into(), json!(path));
            }
            if let Some(layer) = flag(args, "--layer") {
                obj.insert("layer".into(), json!(layer));
            }
            if let Some(view) = flag(args, "--view") {
                if !matches!(view.as_str(), "auto" | "split" | "stack") {
                    return Err(anyhow!("--view must be auto, split, or stack"));
                }
                obj.insert("view".into(), json!(view));
            }
            if let Some(placement) = flag(args, "--placement") {
                if !matches!(placement.as_str(), "preview" | "pane" | "tab") {
                    return Err(anyhow!("--placement must be preview, pane, or tab"));
                }
                obj.insert("placement".into(), json!(placement));
            }
            ("diff.open".into(), Value::Object(obj))
        }
        ("diff", "get") => {
            let path = rest
                .first()
                .filter(|value| !value.starts_with("--"))
                .ok_or_else(|| {
                    anyhow!("usage: luvus diff get <path> [--layer <layer>] [--include-patch]")
                })?;
            let mut obj = serde_json::Map::new();
            obj.insert("path".into(), json!(path));
            if let Some(layer) = flag(args, "--layer") {
                obj.insert("layer".into(), json!(layer));
            }
            if args.iter().any(|arg| arg == "--include-patch") {
                obj.insert("include_patch".into(), json!(true));
            }
            ("diff.get".into(), Value::Object(obj))
        }
        ("diff", "note") => {
            let sub = rest.first().map(String::as_str).unwrap_or("list");
            let positionals: Vec<&String> = {
                let mut values = Vec::new();
                let mut index = 1;
                while index < rest.len() {
                    match rest[index].as_str() {
                        "--yes" if sub == "remove" => index += 1,
                        "--all-open" if sub == "send" => index += 1,
                        flag if matches!(
                            (sub, flag),
                            ("list", "--file" | "--state")
                                | (
                                    "add",
                                    "--file"
                                        | "--body"
                                        | "--old-line"
                                        | "--new-line"
                                        | "--end-line"
                                        | "--layer"
                                        | "--kind"
                                )
                                | ("edit", "--body")
                                | ("send", "--to")
                        ) =>
                        {
                            if rest.get(index + 1).is_none() {
                                return Err(anyhow!("{} requires a value", rest[index]));
                            }
                            index += 2;
                        }
                        flag if flag.starts_with("--") => {
                            return Err(anyhow!("unknown diff note option `{flag}`"));
                        }
                        _ => {
                            values.push(&rest[index]);
                            index += 1;
                        }
                    }
                }
                values
            };
            let line_flag = |name: &str| -> Result<Option<u64>> {
                flag(args, name)
                    .map(|value| {
                        value
                            .parse::<u64>()
                            .ok()
                            .filter(|line| *line > 0 && *line <= u32::MAX as u64)
                            .ok_or_else(|| anyhow!("{name} must be a positive line number"))
                    })
                    .transpose()
            };
            let mut obj = serde_json::Map::new();
            match sub {
                "list" => {
                    if let Some(path) = flag(args, "--file") {
                        obj.insert("file".into(), json!(path));
                    }
                    if let Some(state) = flag(args, "--state") {
                        if !matches!(
                            state.as_str(),
                            "open" | "resolved" | "outdated" | "orphaned"
                        ) {
                            return Err(anyhow!(
                                "--state must be open, resolved, outdated, or orphaned"
                            ));
                        }
                        obj.insert("state".into(), json!(state));
                    }
                    ("diff.note.list".into(), Value::Object(obj))
                }
                "add" => {
                    let file = flag(args, "--file").ok_or_else(|| anyhow!("--file is required"))?;
                    let body = flag(args, "--body").ok_or_else(|| anyhow!("--body is required"))?;
                    obj.insert("file".into(), json!(file));
                    obj.insert("body".into(), json!(body));
                    if let Some(line) = line_flag("--old-line")? {
                        obj.insert("old_line".into(), json!(line));
                    }
                    if let Some(line) = line_flag("--new-line")? {
                        obj.insert("new_line".into(), json!(line));
                    }
                    if let Some(line) = line_flag("--end-line")? {
                        obj.insert("end_line".into(), json!(line));
                    }
                    if let Some(layer) = flag(args, "--layer") {
                        obj.insert("layer".into(), json!(layer));
                    }
                    if let Some(kind) = flag(args, "--kind") {
                        obj.insert("kind".into(), json!(kind));
                    }
                    ("diff.note.add".into(), Value::Object(obj))
                }
                "edit" => {
                    let id = positionals
                        .first()
                        .ok_or_else(|| anyhow!("note id is required"))?;
                    let body = flag(args, "--body").ok_or_else(|| anyhow!("--body is required"))?;
                    obj.insert("id".into(), json!(id));
                    obj.insert("body".into(), json!(body));
                    ("diff.note.edit".into(), Value::Object(obj))
                }
                "resolve" | "reopen" => {
                    let id = positionals
                        .first()
                        .ok_or_else(|| anyhow!("note id is required"))?;
                    obj.insert("id".into(), json!(id));
                    (format!("diff.note.{sub}"), Value::Object(obj))
                }
                "remove" => {
                    if !args.iter().any(|arg| arg == "--yes") {
                        return Err(anyhow!("diff note remove requires --yes"));
                    }
                    let id = positionals
                        .first()
                        .ok_or_else(|| anyhow!("note id is required"))?;
                    obj.insert("id".into(), json!(id));
                    ("diff.note.remove".into(), Value::Object(obj))
                }
                "send" => {
                    let target = flag(args, "--to").ok_or_else(|| anyhow!("--to is required"))?;
                    obj.insert("to".into(), json!(target));
                    let all_open = args.iter().any(|arg| arg == "--all-open");
                    if all_open {
                        obj.insert("all_open".into(), json!(true));
                    }
                    let ids: Vec<_> = positionals.into_iter().cloned().collect();
                    if ids.is_empty() && !all_open {
                        return Err(anyhow!(
                            "diff note send needs at least one note id or --all-open"
                        ));
                    }
                    obj.insert("ids".into(), json!(ids));
                    ("diff.note.send".into(), Value::Object(obj))
                }
                _ => {
                    return Err(anyhow!(
                        "usage: luvus diff note add|list|edit|resolve|reopen|remove|send"
                    ))
                }
            }
        }
        ("diff", other) => {
            return Err(anyhow!(
                "unknown diff command `{other}`. Try `luvus help diff`."
            ))
        }

        ("git", "branches") => ("git.branches".into(), json!({})),
        ("git", "log") => {
            let mut obj = serde_json::Map::new();
            if let Some(n) = flag(args, "--limit").and_then(|s| s.parse::<u64>().ok()) {
                obj.insert("n".to_string(), json!(n));
            }
            ("git.log".into(), Value::Object(obj))
        }
        ("git", "open") => ("git.open".into(), one("workspace", arg0())),
        ("files", "open") => {
            let mut obj = serde_json::Map::new();
            obj.insert("path".to_string(), json!(arg0().unwrap_or_default()));
            if let Some(tg) = flag(args, "--target") {
                obj.insert("target".to_string(), json!(tg));
            }
            ("files.open".into(), Value::Object(obj))
        }
        ("files", "tree") => ("files.tree".into(), json!({})),
        ("files", "reveal") => ("files.reveal".into(), one("path", arg0())),
        ("files", "refresh") => ("files.refresh".into(), json!({})),
        ("files", _) => ("files.tree".into(), json!({})),
        ("git", _) => ("git.status".into(), json!({})),

        ("worktree", "create") => ("worktree.create".into(), one("branch", arg0())),
        ("worktree", "open") => ("worktree.open".into(), one("path", arg0())),
        ("worktree", "remove") => ("worktree.remove".into(), one("path", arg0())),
        ("worktree", _) => ("worktree.list".into(), json!({})),

        // ── orchestration (docs/22, M0): task ledger + path leases ──────────
        ("task", "add") => {
            let title = rest.iter().find(|a| !a.starts_with("--")).cloned();
            let mut obj = serde_json::Map::new();
            obj.insert("title".into(), json!(title.unwrap_or_default()));
            obj.insert("paths".into(), json!(multi_flag(args, "--paths")));
            obj.insert("deps".into(), json!(multi_flag(args, "--dep")));
            if let Some(g) = flag(args, "--gate") {
                obj.insert("gate".into(), json!(g));
            }
            ("task.add".into(), Value::Object(obj))
        }
        ("task", "get") => ("task.get".into(), one("id", arg0())),
        ("task", "next") => {
            let mut obj = serde_json::Map::new();
            if args.iter().any(|a| a == "--start") {
                obj.insert("start".into(), json!(true));
            }
            if let Some(a) = flag(args, "--agent") {
                obj.insert("agent".into(), json!(a));
            }
            let pv = pane();
            if !pv.is_null() {
                obj.insert("pane".into(), pv);
            }
            ("task.next".into(), Value::Object(obj))
        }
        ("task", "heartbeat") => {
            let mut obj = serde_json::Map::new();
            if let Some(id) = arg0() {
                obj.insert("id".into(), json!(id));
            }
            if let Some(c) = flag(args, "--context").and_then(|s| s.parse::<f64>().ok()) {
                obj.insert("context".into(), json!(c));
            }
            ("task.heartbeat".into(), Value::Object(obj))
        }
        ("task", "start") => {
            let mut obj = serde_json::Map::new();
            if let Some(id) = arg0() {
                obj.insert("id".into(), json!(id));
            }
            if let Some(b) = flag(args, "--branch") {
                obj.insert("branch".into(), json!(b));
            }
            if let Some(a) = flag(args, "--agent") {
                obj.insert("agent".into(), json!(a));
            }
            ("task.start".into(), Value::Object(obj))
        }
        ("task", "claim") => {
            let mut obj = serde_json::Map::new();
            if let Some(id) = arg0() {
                obj.insert("id".into(), json!(id));
            }
            let pv = pane();
            if !pv.is_null() {
                obj.insert("pane".into(), pv);
            }
            ("task.claim".into(), Value::Object(obj))
        }
        ("task", "done") => ("task.done".into(), one("id", arg0())),
        ("task", "delete") => ("task.delete".into(), one("id", arg0())),
        ("task", "merge") => ("task.merge".into(), one("id", arg0())),
        ("task", "release") => ("task.release".into(), one("id", arg0())),
        ("task", "update") => {
            let mut obj = serde_json::Map::new();
            if let Some(id) = arg0() {
                obj.insert("id".into(), json!(id));
            }
            if let Some(s) = flag(args, "--status") {
                obj.insert("status".into(), json!(s));
            }
            if let Some(o) = flag(args, "--output") {
                obj.insert("output".into(), json!(o));
            }
            if let Some(n) = flag(args, "--note") {
                obj.insert("note".into(), json!(n));
            }
            ("task.update".into(), Value::Object(obj))
        }
        ("task", _) => ("task.list".into(), json!({})),

        ("lease", "acquire") => {
            // Positional paths up to the first flag, plus `--task <id>`.
            let paths: Vec<String> = rest
                .iter()
                .take_while(|a| !a.starts_with("--"))
                .cloned()
                .collect();
            let mut obj = serde_json::Map::new();
            obj.insert("paths".into(), json!(paths));
            if let Some(t) = flag(args, "--task") {
                obj.insert("task".into(), json!(t));
            }
            let pv = pane();
            if !pv.is_null() {
                obj.insert("pane".into(), pv);
            }
            ("lease.acquire".into(), Value::Object(obj))
        }
        ("lease", "release") => ("lease.release".into(), one("id", arg0())),
        ("lease", _) => ("lease.list".into(), json!({})),

        _ => return Err(anyhow!("unknown command. Try `luvus --help`.")),
    })
}

/// Collect values that follow every occurrence of `--name` up to the next flag,
/// e.g. `--paths a b c` → `[a,b,c]`, and repeated `--dep t1 --dep t2` → `[t1,t2]`.
fn multi_flag(args: &[String], name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == name {
            let mut j = i + 1;
            while j < args.len() && !args[j].starts_with("--") {
                out.push(args[j].clone());
                j += 1;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// Value following `--name` in argv, if present.
fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

/// A module-setting value typed on the command line. `true`/`false` and whole
/// numbers become JSON scalars so a bool/number setting takes them directly;
/// everything else stays a string (the server coerces against the declared
/// type either way).
fn parse_setting_value(s: &str) -> Value {
    match s {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => match s.parse::<i64>() {
            Ok(n) => Value::from(n),
            Err(_) => Value::String(s.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    fn rendered_topic_help(topic: &str, command: Option<&str>) -> String {
        let mut output = Vec::new();
        assert!(write_topic_help(&mut output, topic, command).unwrap());
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn family_help_lists_only_commands_owned_by_that_family() {
        for (topic, noun) in [
            ("workspace", "workspace"),
            ("tab", "tab"),
            ("pane", "pane"),
            ("agent", "agent"),
            ("skill", "skill"),
            ("wait", "wait"),
            ("attach", "attach"),
            ("search", "search"),
            ("theme", "theme"),
            ("bar", "bar"),
            ("ui", "ui"),
            ("module", "module"),
            ("git", "git"),
            ("files", "files"),
            ("diff", "diff"),
            ("worktree", "worktree"),
            ("task", "task"),
            ("lease", "lease"),
            ("events", "events"),
            ("uhp", "uhp"),
            ("remote", "--remote"),
            ("server", "server"),
            ("integration", "integration"),
        ] {
            let output = rendered_topic_help(topic, None);
            assert!(output.ends_with(HELP_BUG), "{topic} help lost the bug");
            let rows: Vec<_> = output
                .lines()
                .filter(|line| {
                    line.starts_with("  ")
                        && line
                            .as_bytes()
                            .get(2)
                            .is_some_and(|character| *character != b' ')
                })
                .collect();
            assert!(!rows.is_empty(), "{topic} help has no command rows");
            assert!(
                rows.iter().all(|line| {
                    line.trim_start()
                        .strip_prefix(noun)
                        .is_some_and(|rest| rest.chars().next().is_some_and(char::is_whitespace))
                }),
                "{topic} help contains another family: {rows:?}"
            );
        }
    }

    #[test]
    fn leaf_help_still_selects_only_the_requested_command() {
        let output = rendered_topic_help("pane", Some("split"));
        assert!(output.contains("pane split [<id>]"));
        assert!(!output.contains("pane list"));
        assert!(!output.contains("agent start"));
    }

    #[test]
    fn pass_through_commands_preserve_trailing_help_payloads() {
        for raw in [
            "luvus pane run 9 cargo --help",
            "luvus pane run 9 cargo -h",
            "luvus pane send 9 --help",
            "luvus pane send 9 -h",
            "luvus agent send reviewer --help",
            "luvus agent send reviewer -h",
            "luvus agent prompt reviewer --help",
            "luvus agent prompt reviewer -h",
            "luvus agent keys reviewer --help",
            "luvus agent keys reviewer -h",
            "luvus --remote devbox --help",
            "luvus --remote devbox -h",
        ] {
            let args = argv(raw);
            assert_eq!(command_help_request(&args), None, "{raw}");
        }
    }

    #[test]
    fn uhp_is_the_single_public_protocol_cli_route() {
        assert!(is_cli(&argv("luvus uhp capabilities")));
        assert_eq!(
            parse(&argv("luvus uhp capabilities")).unwrap().0,
            "uhp.capabilities"
        );
        assert_eq!(
            parse(&argv("luvus uhp snapshot")).unwrap().0,
            "session.snapshot"
        );
        assert_eq!(
            parse(&argv("luvus uhp events")).unwrap().0,
            "events.subscribe"
        );
        assert!(parse(&argv("luvus uhp capabilities extra")).is_err());
        assert_eq!(
            parse(&argv("luvus socket capabilities"))
                .unwrap_err()
                .to_string(),
            "unknown command. Try `luvus --help`."
        );
    }

    #[test]
    fn unreleased_api_aliases_are_rejected() {
        assert!(is_cli(&argv("luvus api schema")));
        assert_eq!(
            parse(&argv("luvus api schema")).unwrap_err().to_string(),
            "unknown command. Try `luvus --help`."
        );
    }

    #[test]
    fn agent_authority_and_process_commands_map_to_structured_api() {
        let (method, params) = parse(&argv(
            "luvus agent report 7 --source fx/plugin --kind fx --status working --message busy --sequence 4 --ttl 30",
        ))
        .unwrap();
        assert_eq!(method, "agent.report");
        assert_eq!(params["pane"], "7");
        assert_eq!(params["source"], "fx/plugin");
        assert_eq!(params["agent"], "fx");
        assert_eq!(params["status"], "working");
        assert_eq!(params["sequence"], 4);
        assert_eq!(params["ttl_s"], 30);
        assert_eq!(
            parse(&argv("luvus agent explain 7")).unwrap().0,
            "agent.explain"
        );
        assert_eq!(
            parse(&argv("luvus agent release 7 --source fx/plugin"))
                .unwrap()
                .0,
            "agent.release"
        );
        assert_eq!(
            parse(&argv("luvus pane processes 7")).unwrap().0,
            "pane.processes"
        );
    }

    #[test]
    fn normal_and_group_help_detection_remains_intact() {
        for (raw, expected) in [
            ("luvus pane --help", Some(("pane", None))),
            ("luvus --remote --help", Some(("--remote", None))),
            ("luvus pane split --help", Some(("pane", Some("split")))),
            ("luvus module install -h", Some(("module", Some("install")))),
            ("luvus update --help", Some(("update", None))),
        ] {
            let args = argv(raw);
            assert_eq!(command_help_request(&args), expected, "{raw}");
        }
    }

    #[test]
    fn update_is_a_top_level_local_cli_command() {
        assert!(is_cli(&argv("luvus update")));
        assert!(!help_topic_has_subcommands("update"));
    }

    #[test]
    fn theme_cli_is_recognized_and_validates_command_shapes() {
        assert!(is_cli(&argv("luvus theme list")));
        assert_eq!(
            command_help_request(&argv("luvus theme install --help")),
            Some(("theme", Some("install")))
        );
        for invalid in [
            vec!["list".into(), "extra".into()],
            vec!["path".into(), "extra".into()],
            vec![
                "init".into(),
                "new-theme".into(),
                "--bad".into(),
                "noir".into(),
            ],
            vec!["validate".into(), "x.toml".into(), "--bad".into()],
            vec!["install".into(), "x.toml".into(), "--bad".into()],
            vec!["use".into(), "noir".into(), "extra".into()],
            vec!["uninstall".into(), "x".into(), "extra".into()],
            vec!["reload".into(), "extra".into()],
        ] {
            assert!(theme_cmd(&invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn maps_commands() {
        std::env::remove_var("LUVUS_PANE_ID");
        let (m, _) = parse(&argv("luvus ping")).unwrap();
        assert_eq!(m, "ping");

        let (m, _) = parse(&argv("luvus pane list")).unwrap();
        assert_eq!(m, "pane.list");

        let (m, p) = parse(&argv("luvus pane split --down")).unwrap();
        assert_eq!(m, "pane.split");
        assert_eq!(p.get("direction").and_then(|v| v.as_str()), Some("down"));

        let (m, p) = parse(&argv("luvus pane run 3 echo hi")).unwrap();
        assert_eq!(m, "pane.run");
        assert_eq!(p.get("pane").and_then(|v| v.as_str()), Some("3"));
        assert_eq!(p.get("command").and_then(|v| v.as_str()), Some("echo hi"));

        let (m, _) = parse(&argv("luvus node list")).unwrap();
        assert_eq!(m, "workspace.list");
        let (m, p) = parse(&argv("luvus node focus 2")).unwrap();
        assert_eq!(m, "workspace.focus");
        assert_eq!(p.get("workspace").and_then(|v| v.as_str()), Some("2"));
        let (m, _) = parse(&argv("luvus tab new")).unwrap();
        assert_eq!(m, "tab.new");
        let (m, _) = parse(&argv("luvus agent list")).unwrap();
        assert_eq!(m, "agent.list");
    }

    #[test]
    fn search_keeps_exact_contract_and_fuzzy_is_explicit() {
        let (method, params) = parse(&argv("luvus search build failed --case")).unwrap();
        assert_eq!(method, "search");
        assert_eq!(
            params,
            json!({"query": "build failed", "case_sensitive": true})
        );

        let (method, params) =
            parse(&argv("luvus search --fuzzy -- --case --scope output")).unwrap();
        assert_eq!(method, "search.query");
        assert_eq!(
            params,
            json!({
                "query": "--case --scope output",
                "scope": "all",
                "all_sessions": false,
                "limit": 200,
                "case_sensitive": false,
            })
        );

        let (method, params) = parse(&argv("luvus search -- --fuzzy --case")).unwrap();
        assert_eq!(method, "search");
        assert_eq!(
            params,
            json!({"query": "--fuzzy --case", "case_sensitive": false})
        );

        let (method, params) = parse(&argv(
            "luvus search --fuzzy api auth --scope files --all-sessions --limit 25 --case --json",
        ))
        .unwrap();
        assert_eq!(method, "search.query");
        assert_eq!(
            params,
            json!({
                "query": "api auth",
                "scope": "files",
                "all_sessions": true,
                "limit": 25,
                "case_sensitive": true,
            })
        );

        for bad in [
            "luvus search --fuzzy",
            "luvus search --fuzzy api --scope unknown",
            "luvus search --fuzzy api --limit 0",
            "luvus search --fuzzy api --limit 201",
            "luvus search api --all-sessions",
            "luvus search api --unknown",
        ] {
            assert!(parse(&argv(bad)).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn maps_workspace_organization_and_rejects_bad_syntax() {
        let (method, params) = parse(&argv("luvus workspace rename 2 Luvus website")).unwrap();
        assert_eq!(method, "workspace.rename");
        assert_eq!(params, json!({"workspace": "2", "name": "Luvus website"}));

        let (method, params) = parse(&argv("luvus workspace pin 0")).unwrap();
        assert_eq!(method, "workspace.pin");
        assert_eq!(params, json!({"workspace": "0", "pinned": true}));

        let (method, params) = parse(&argv("luvus node unpin 3")).unwrap();
        assert_eq!(method, "workspace.pin");
        assert_eq!(params, json!({"workspace": "3", "pinned": false}));

        for bad in [
            "luvus workspace rename",
            "luvus workspace rename 2",
            "luvus workspace rename two name",
            "luvus workspace pin",
            "luvus workspace pin -1",
            "luvus workspace pin 1 extra",
            "luvus workspace unpin two",
            "luvus workspace organize 1",
        ] {
            assert!(parse(&argv(bad)).is_err(), "{bad} must be rejected");
        }

        let long = format!("luvus workspace rename 1 {}", "x".repeat(41));
        assert!(
            parse(&argv(&long)).is_err(),
            "41-character name is rejected"
        );
    }

    #[test]
    fn maps_pane_and_tab_moves_and_rejects_bad_syntax() {
        let (m, p) = parse(&argv("luvus pane move 7 --tab 3")).unwrap();
        assert_eq!(m, "pane.move");
        assert_eq!(p["pane"], "7");
        assert_eq!(p["tab"], "3");
        assert!(p.get("new_tab").is_none());

        let (m, p) = parse(&argv("luvus pane move 7 --new-tab")).unwrap();
        assert_eq!(m, "pane.move");
        assert_eq!(p["pane"], "7");
        assert_eq!(p["new_tab"], true);
        assert!(p.get("tab").is_none());

        let (m, p) = parse(&argv("luvus tab move 3 1")).unwrap();
        assert_eq!(m, "tab.move");
        assert_eq!(p, json!({"tab": "3", "to": "1"}));

        let (m, p) = parse(&argv("luvus tab move left")).unwrap();
        assert_eq!(m, "tab.move");
        assert_eq!(p, json!({"direction": "left"}));

        let (m, p) = parse(&argv("luvus tab move right --tab 3")).unwrap();
        assert_eq!(m, "tab.move");
        assert_eq!(p, json!({"direction": "right", "tab": "3"}));

        let (m, p) = parse(&argv("luvus tab focus 2")).unwrap();
        assert_eq!(m, "tab.focus");
        assert_eq!(p, json!({"tab": "2"}));

        let (m, p) = parse(&argv("luvus tab swap 1 3")).unwrap();
        assert_eq!(m, "tab.swap");
        assert_eq!(p, json!({"tab": "1", "with": "3"}));

        for bad in [
            "luvus pane move 7",
            "luvus pane move 7 --tab 0",
            "luvus pane move 7 --tab 2 --new-tab",
            "luvus pane move worker --tab 2",
            "luvus pane move 7 8 --tab 2",
            "luvus pane move 7 --new-tab extra",
            "luvus pane move 7 --where 2",
            "luvus tab move 1",
            "luvus tab move 0 1",
            "luvus tab move 1 two",
            "luvus tab move left --tab 0",
            "luvus tab move right --tab",
            "luvus tab move left right",
            "luvus tab move up",
            "luvus tab swap 1",
            "luvus tab swap 0 1",
            "luvus tab swap 1 two",
            "luvus tab swap 2 2",
            "luvus tab swap 1 2 extra",
            "luvus tab focus",
            "luvus tab focus 0",
            "luvus tab focus two",
            "luvus tab focus 1 extra",
            "luvus pane teleport 7",
            "luvus tab reorder 2 1",
        ] {
            assert!(parse(&argv(bad)).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn tab_rename_validates_optional_target_and_name_length() {
        let (method, params) = parse(&argv("luvus tab rename review notes --tab 2")).unwrap();
        assert_eq!(method, "tab.rename");
        assert_eq!(params, json!({"name": "review notes", "tab": "2"}));

        let (method, params) = parse(&argv("luvus tab rename")).unwrap();
        assert_eq!(method, "tab.rename");
        assert_eq!(params, json!({"name": ""}));

        for bad in [
            "luvus tab rename review --tab 0",
            "luvus tab rename review --tab nope",
            "luvus tab rename review --tab",
            "luvus tab rename review --tab 1 --tab 2",
            "luvus tab rename review --unknown value",
        ] {
            assert!(parse(&argv(bad)).is_err(), "{bad} must be rejected");
        }

        let long = format!("luvus tab rename {}", "x".repeat(41));
        assert!(
            parse(&argv(&long)).is_err(),
            "41-character name is rejected"
        );
    }

    #[test]
    fn maps_agent_fork_and_rejects_ambiguous_syntax() {
        let (method, params) = parse(&argv(
            "luvus agent fork reviewer --name experiment --no-focus",
        ))
        .unwrap();
        assert_eq!(method, "agent.fork");
        assert_eq!(
            params,
            json!({"target": "reviewer", "name": "experiment", "focus": false})
        );

        let (method, params) = parse(&argv("luvus agent fork 7")).unwrap();
        assert_eq!(method, "agent.fork");
        assert_eq!(params, json!({"target": "7"}));

        for bad in [
            "luvus agent fork",
            "luvus agent fork --no-focus",
            "luvus agent fork 7 extra",
            "luvus agent fork 7 --name",
            "luvus agent fork 7 --name one --name two",
            "luvus agent fork 7 --no-focus --no-focus",
            "luvus agent fork 7 --down",
        ] {
            assert!(parse(&argv(bad)).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn maps_diff_review_commands() {
        assert!(is_cli(&argv("luvus diff list")));
        let (method, params) = parse(&argv("luvus diff list --layer staged")).unwrap();
        assert_eq!(method, "diff.list");
        assert_eq!(params, json!({"layer":"staged"}));

        let (method, params) = parse(&argv(
            "luvus diff open src/lib.rs --layer worktree --view split --placement pane",
        ))
        .unwrap();
        assert_eq!(method, "diff.open");
        assert_eq!(params["path"], "src/lib.rs");
        assert_eq!(params["view"], "split");
        assert_eq!(params["placement"], "pane");

        let (method, params) = parse(&argv("luvus diff get src/lib.rs --include-patch")).unwrap();
        assert_eq!(method, "diff.get");
        assert_eq!(params, json!({"path":"src/lib.rs","include_patch":true}));

        let (method, params) = parse(&argv(
            "luvus diff note add --file src/lib.rs --new-line 12 --end-line 14 --body check",
        ))
        .unwrap();
        assert_eq!(method, "diff.note.add");
        assert_eq!(params["new_line"], 12);
        assert_eq!(params["end_line"], 14);

        let (method, params) = parse(&argv("luvus diff note edit --body updated n1")).unwrap();
        assert_eq!(method, "diff.note.edit");
        assert_eq!(params, json!({"id":"n1","body":"updated"}));

        let (method, params) = parse(&argv("luvus diff note remove --yes n1")).unwrap();
        assert_eq!(method, "diff.note.remove");
        assert_eq!(params, json!({"id":"n1"}));

        let (method, params) = parse(&argv("luvus diff note send --to reviewer n1 n2")).unwrap();
        assert_eq!(method, "diff.note.send");
        assert_eq!(params["to"], "reviewer");
        assert_eq!(params["ids"], json!(["n1", "n2"]));

        let (method, params) = parse(&argv("luvus diff note send same --to same")).unwrap();
        assert_eq!(method, "diff.note.send");
        assert_eq!(params["to"], "same");
        assert_eq!(params["ids"], json!(["same"]));

        let (method, params) =
            parse(&argv("luvus diff note send --to reviewer --all-open")).unwrap();
        assert_eq!(method, "diff.note.send");
        assert_eq!(params["all_open"], true);
        assert_eq!(params["ids"], json!([]));

        for bad in [
            "luvus diff open src/lib.rs --view columns",
            "luvus diff open src/lib.rs --placement replace",
            "luvus diff note add --file src/lib.rs --new-line zero --body check",
            "luvus diff note list --state unknown",
            "luvus diff note remove n1",
            "luvus diff note send n1",
            "luvus diff note send --to reviewer",
            "luvus diff note edit n1 --state open --body check",
            "luvus diff note edit n1 --body",
        ] {
            assert!(parse(&argv(bad)).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn maps_sidebar_dock_commands() {
        std::env::remove_var("LUVUS_PANE_ID");

        // A dock push with inline rows (docs/29) — the plugin API.
        let av: Vec<String> = vec![
            "luvus".into(),
            "ui".into(),
            "dock".into(),
            "push".into(),
            "--id".into(),
            "you.ci".into(),
            "--title".into(),
            "CI".into(),
            "--side".into(),
            "right".into(),
            "--rows".into(),
            r#"[{"text":"build ok","dot":"done","action":"open"}]"#.into(),
        ];
        let (m, p) = parse(&av).unwrap();
        assert_eq!(m, "ui.dock.push");
        assert_eq!(p.get("id").and_then(|v| v.as_str()), Some("you.ci"));
        assert_eq!(p.get("title").and_then(|v| v.as_str()), Some("CI"));
        assert_eq!(p.get("placement").and_then(|v| v.as_str()), Some("right"));
        let rows = p.get("rows").and_then(|v| v.as_array()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get("text").and_then(|v| v.as_str()),
            Some("build ok")
        );

        // Missing --id is a clear error.
        assert!(parse(&argv("luvus ui dock push")).is_err());

        let (m, p) = parse(&argv("luvus ui dock move --id you.ci --side left")).unwrap();
        assert_eq!(m, "ui.dock.move");
        assert_eq!(p.get("side").and_then(|v| v.as_str()), Some("left"));

        let (m, _) = parse(&argv("luvus ui dock list")).unwrap();
        assert_eq!(m, "ui.dock.list");

        // `ui sidebar` now takes an optional side.
        let (m, p) = parse(&argv("luvus ui sidebar --side right --width 30")).unwrap();
        assert_eq!(m, "ui.sidebar");
        assert_eq!(p.get("side").and_then(|v| v.as_str()), Some("right"));
    }

    #[test]
    fn maps_luvus_bar_and_notification_commands() {
        let _env = crate::persist::test_env("cli-bar");
        std::env::set_var("LUVUS_MODULE_ID", "you.ci");
        let args = vec![
            "luvus".into(),
            "bar".into(),
            "push".into(),
            "--id".into(),
            "status".into(),
            "--region".into(),
            "top-right".into(),
            "--content".into(),
            r#"[{"type":"text","text":"CI"},{"type":"state","state":"done"}]"#.into(),
        ];
        let (method, params) = parse(&args).unwrap();
        assert_eq!(method, "ui.bar.push");
        assert_eq!(params["owner"], "you.ci");
        assert_eq!(params["content"].as_array().unwrap().len(), 2);

        let (method, params) =
            parse(&argv("luvus bar move --id status --region bottom-right")).unwrap();
        assert_eq!(method, "ui.bar.move");
        assert_eq!(params["region"], "bottom-right");

        let (method, params) = parse(&argv(
            "luvus ui notification push --text failed --level error --ttl-ms 6000",
        ))
        .unwrap();
        assert_eq!(method, "ui.notification.push");
        assert_eq!(params["ttl_ms"], 6000);
        assert_eq!(params["owner"], "you.ci");
        std::env::remove_var("LUVUS_MODULE_ID");

        for bad in [
            "luvus bar push --id status",
            "luvus bar move --id status --region middle",
            "luvus ui bar list",
            "luvus ui notification push --text bad --level urgent",
        ] {
            assert!(parse(&argv(bad)).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn maps_orchestration_commands() {
        std::env::remove_var("LUVUS_PANE_ID");

        let (m, p) = parse(&argv(
            "luvus task add title --paths src/auth src/api --dep t1 --gate cargo",
        ))
        .unwrap();
        assert_eq!(m, "task.add");
        assert_eq!(p.get("title").and_then(|v| v.as_str()), Some("title"));
        assert_eq!(
            p.get("paths").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(2)
        );
        assert_eq!(
            p.get("deps").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(1)
        );
        assert_eq!(p.get("gate").and_then(|v| v.as_str()), Some("cargo"));

        let (m, _) = parse(&argv("luvus task list")).unwrap();
        assert_eq!(m, "task.list");
        let (m, p) = parse(&argv("luvus task claim t3")).unwrap();
        assert_eq!(m, "task.claim");
        assert_eq!(p.get("id").and_then(|v| v.as_str()), Some("t3"));
        let (m, _) = parse(&argv("luvus task done t3")).unwrap();
        assert_eq!(m, "task.done");

        let (m, p) = parse(&argv("luvus lease acquire src/auth/** --task t1")).unwrap();
        assert_eq!(m, "lease.acquire");
        assert_eq!(
            p.get("paths").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(1)
        );
        assert_eq!(p.get("task").and_then(|v| v.as_str()), Some("t1"));

        // ORCH-3/4/5/6 verbs.
        let (m, p) = parse(&argv("luvus task start t1 --branch feat --agent claude")).unwrap();
        assert_eq!(m, "task.start");
        assert_eq!(p.get("branch").and_then(|v| v.as_str()), Some("feat"));
        assert_eq!(p.get("agent").and_then(|v| v.as_str()), Some("claude"));

        let (m, p) = parse(&argv("luvus task next --start --agent claude")).unwrap();
        assert_eq!(m, "task.next");
        assert_eq!(p.get("start").and_then(|v| v.as_bool()), Some(true));

        let (m, p) = parse(&argv("luvus task heartbeat t1 --context 0.7")).unwrap();
        assert_eq!(m, "task.heartbeat");
        assert_eq!(p.get("context").and_then(|v| v.as_f64()), Some(0.7));

        let (m, p) = parse(&argv("luvus task merge t1")).unwrap();
        assert_eq!(m, "task.merge");
        assert_eq!(p.get("id").and_then(|v| v.as_str()), Some("t1"));
    }

    #[test]
    fn maps_module_commands() {
        let (m, _) = parse(&argv("luvus module list")).unwrap();
        assert_eq!(m, "module.list");

        let (m, p) = parse(&argv("luvus module link ./mod --disabled")).unwrap();
        assert_eq!(m, "module.link");
        assert_eq!(p.get("path").and_then(|v| v.as_str()), Some("./mod"));
        assert_eq!(p.get("disabled").and_then(|v| v.as_bool()), Some(true));

        let (m, p) = parse(&argv("luvus module run my-mod refresh")).unwrap();
        assert_eq!(m, "module.action.invoke");
        assert_eq!(p.get("module").and_then(|v| v.as_str()), Some("my-mod"));
        assert_eq!(p.get("id").and_then(|v| v.as_str()), Some("refresh"));

        let (m, p) = parse(&argv("luvus module run refresh")).unwrap();
        assert_eq!(m, "module.action.invoke");
        assert_eq!(p.get("id").and_then(|v| v.as_str()), Some("refresh"));
        assert!(p.get("module").is_none());

        let (m, p) = parse(&argv("luvus module enable my-mod")).unwrap();
        assert_eq!(m, "module.enable");
        assert_eq!(p.get("id").and_then(|v| v.as_str()), Some("my-mod"));
    }

    #[test]
    fn parses_wait() {
        std::env::remove_var("LUVUS_PANE_ID");
        let s = parse_wait(&argv("luvus wait output 3 --match done --timeout 5")).unwrap();
        assert_eq!(s.pane, "3");
        assert_eq!(s.timeout, Some(5.0));
        assert_eq!(
            s.condition,
            WaitFor::Output {
                needle: "done".into()
            }
        );

        let s = parse_wait(&argv("luvus wait agent-status 7 --status blocked")).unwrap();
        assert_eq!(s.pane, "7");
        assert_eq!(s.timeout, None);
        assert_eq!(
            s.condition,
            WaitFor::AgentStatus {
                status: "blocked".into()
            }
        );

        // missing --match is an error
        assert!(parse_wait(&argv("luvus wait output 3")).is_err());
        // pane id falls back to $LUVUS_PANE_ID
        std::env::set_var("LUVUS_PANE_ID", "9");
        let s = parse_wait(&argv("luvus wait output --match hi")).unwrap();
        assert_eq!(s.pane, "9");
        std::env::remove_var("LUVUS_PANE_ID");
    }

    #[test]
    fn parses_agent_start_target() {
        assert_eq!(
            parse_agent_start_target(
                &argv("luvus agent start worker --kind codex --pane 9"),
                Some("2".into())
            )
            .unwrap(),
            AgentStartTarget::Existing("9".into())
        );
        assert_eq!(
            parse_agent_start_target(
                &argv("luvus agent start worker --kind codex --anchor 4 --down"),
                None
            )
            .unwrap(),
            AgentStartTarget::Split {
                anchor: Some("4".into()),
                down: true,
            }
        );
        assert_eq!(
            parse_agent_start_target(
                &argv("luvus agent start worker --kind codex"),
                Some("7".into())
            )
            .unwrap(),
            AgentStartTarget::Split {
                anchor: Some("7".into()),
                down: false,
            }
        );
        assert!(parse_agent_start_target(
            &argv("luvus agent start worker --kind codex --pane 9 --anchor 4"),
            None
        )
        .is_err());
    }

    #[test]
    fn maps_git_commands() {
        let (m, _) = parse(&argv("luvus git status")).unwrap();
        assert_eq!(m, "git.status");
        let (m, _) = parse(&argv("luvus git")).unwrap();
        assert_eq!(m, "git.status");
        let (m, _) = parse(&argv("luvus git branches")).unwrap();
        assert_eq!(m, "git.branches");
        let (m, p) = parse(&argv("luvus git log --limit 5")).unwrap();
        assert_eq!(m, "git.log");
        assert_eq!(p.get("n").and_then(|v| v.as_u64()), Some(5));
        let (m, p) = parse(&argv("luvus git open 2")).unwrap();
        assert_eq!(m, "git.open");
        assert_eq!(p.get("workspace").and_then(|v| v.as_str()), Some("2"));
    }

    #[test]
    fn maps_worktree_commands() {
        let (m, _) = parse(&argv("luvus worktree list")).unwrap();
        assert_eq!(m, "worktree.list");
        let (m, p) = parse(&argv("luvus worktree create feature/x")).unwrap();
        assert_eq!(m, "worktree.create");
        assert_eq!(p.get("branch").and_then(|v| v.as_str()), Some("feature/x"));
        let (m, p) = parse(&argv("luvus worktree open /tmp/wt")).unwrap();
        assert_eq!(m, "worktree.open");
        assert_eq!(p.get("path").and_then(|v| v.as_str()), Some("/tmp/wt"));
        let (m, p) = parse(&argv("luvus worktree remove /tmp/wt")).unwrap();
        assert_eq!(m, "worktree.remove");
        assert_eq!(p.get("path").and_then(|v| v.as_str()), Some("/tmp/wt"));
    }

    #[test]
    fn unsupported_keys_explain_wsl_repairs_without_calling_luvus_broken() {
        let (detail, action) = unsupported_key_guidance(true, true);
        assert!(detail.contains("all other features still work"));
        assert!(action.contains("Windows Terminal to 1.25+"));
        assert!(action.contains("ESC CR"));

        let (detail, action) = unsupported_key_guidance(true, false);
        assert!(detail.starts_with("WSL detected"));
        assert!(action.contains("Windows Terminal 1.25+"));

        let (detail, action) = unsupported_key_guidance(false, false);
        assert!(detail.contains("only the modified-Enter shortcut is affected"));
        assert!(action.contains("Alt/Option+Enter"));
    }

    // The docs site's CLI reference (website/…/reference/cli.mdx) carries the
    // `luvus help all` text VERBATIM — this guard fails CI if a command changes
    // without the docs page being regenerated, so the two can never drift.
    // Regenerate with:  luvus help all  →  the page's ```txt block.
    #[test]
    fn unknown_method_detection_is_method_specific() {
        let response = json!({"error": {
            "code": "invalid_request",
            "message": "unknown method: theme.use"
        }});
        assert!(is_unknown_method(&response, "theme.use"));
        assert!(!is_unknown_method(&response, "theme.reload"));
        assert!(!is_unknown_method(
            &json!({"error": {"message": "theme.use failed"}}),
            "theme.use"
        ));
    }

    #[test]
    fn api_proxy_exit_code_distinguishes_protocol_errors() {
        assert_eq!(api_response_exit_code(r#"{"id":"1","result":{}}"#), 0);
        assert_eq!(
            api_response_exit_code(
                r#"{"id":"1","error":{"code":"invalid_request","message":"bad"}}"#
            ),
            1
        );
        assert_eq!(api_response_exit_code("not json"), 0);
    }

    #[test]
    fn api_proxy_rejects_mismatched_response_ids() {
        assert!(validate_response_id(
            r#"{"id":"request-1","method":"ping","params":{}}"#,
            r#"{"id":"request-1","result":{}}"#,
        )
        .is_ok());
        assert!(validate_response_id(
            r#"{"id":"request-1","method":"ping","params":{}}"#,
            r#"{"id":"request-2","result":{}}"#,
        )
        .is_err());
    }

    #[test]
    fn docs_cli_reference_matches_help() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("website/src/content/docs/docs/reference/cli.mdx");
        let Ok(page) = std::fs::read_to_string(&path) else {
            return; // published crate / partial checkout — nothing to check
        };
        let block = page
            .split("```txt\n")
            .nth(1)
            .and_then(|rest| rest.split("\n```").next())
            .expect("cli.mdx must contain a ```txt block");
        // DETAILED_USAGE ends with a newline the split consumes; compare trimmed.
        assert_eq!(
            block.trim_end(),
            format!("{DETAILED_USAGE}{HELP_BUG}").trim_end(),
            "website/…/reference/cli.mdx has drifted from `luvus help all` — \
             regenerate the page's txt block from DETAILED_USAGE plus HELP_BUG"
        );
    }

    #[test]
    fn agent_docs_are_published_and_linked_from_help() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let readme = std::fs::read_to_string(root.join("website/public/agent-readme.md"));
        let llms = std::fs::read_to_string(root.join("website/public/llms.txt"));
        let (Ok(readme), Ok(llms)) = (readme, llms) else {
            return; // published crate / partial checkout
        };
        assert!(HELP_BUG.contains("https://luvus.dev/agent-readme.md"));
        assert!(!HELP_BUG.contains("https://luvus.dev/llms.txt"));
        assert!(readme.starts_with("# Luvus README for AI agents\n"));
        assert!(readme.contains("luvus uhp capabilities"));
        assert!(llms.starts_with("# Luvus knowledge map for language models\n"));
        assert!(llms.contains("https://luvus.dev/docs/reference/api/"));
    }

    #[test]
    fn skill_management_exposes_one_bundled_skill() {
        let _env = crate::persist::test_env("cli-skill-opt-in");
        assert_eq!(skill_cmd(&[]).unwrap(), 0);
        assert_eq!(skill_cmd(&["status".into()]).unwrap(), 0);
        assert_eq!(skill_cmd(&["show".into()]).unwrap(), 0);

        for args in [
            vec!["enable".into(), "codex".into()],
            vec!["disable".into(), "--all".into()],
            vec!["status".into(), "claude".into()],
            vec!["show".into(), "opencode".into()],
            vec!["update".into()],
            vec!["update".into(), "codex".into()],
            vec!["on".into()],
            vec!["install".into()],
        ] {
            assert!(skill_cmd(&args).is_err(), "{args:?}");
        }
    }
}
