---
name: luvus
description: "Control Luvus through its local CLI and UHP. Use only for a line beginning with `=target message`, an explicit request naming Luvus, a request to delegate to a named live Luvus agent or pane, or an explicit Luvus operation involving sessions, workspaces, tabs, panes, agents, files, Git, DIFF, worktrees, tasks, leases, modules, themes, Luvus Bar, configuration, UI, integrations, or Luvus UHP. Do not use for ordinary coding, file edits, Git operations, tests, task planning, generic agent work, or parallelization unless the user explicitly connects the request to Luvus. Being inside Luvus does not trigger this skill by itself. Inside Luvus use the inherited session; outside use the installed production Luvus command and configured session."
---

# Luvus

Use Luvus's semantic CLI and UHP to delegate work, control the live workspace,
and build explicit harness integrations. The skill adds no service, event loop,
or polling process.

## Route `=target` delegation first

Treat `=target message` as Luvus delegation only when `=` is the first
non-whitespace character on a line, `target` contains no spaces, and a message
follows it. Do not treat equations, assignments, or `=` in prose as delegation.
Codex skill invocations keep their `$skill-name` syntax.

Examples:

- `=reviewer inspect this diff` sends `inspect this diff` to the agent named
  `reviewer`.
- `=7 run the migration` addresses pane 7.
- `=codex add tests` addresses the unique agent named or kinded `codex`.

For every delegation line:

1. Run `luvus agent send <target> "<message>"` directly without `--wait`.
   `agent send` resolves live names, numeric pane ids, and unique agent kinds
   authoritatively. Do not run `agent list` first.
2. On success, accept the returned pane, agent, name, and status, tell the user
   where the work was sent, and end the turn. Do not reread or poll.
3. Only after `not_found` or `ambiguous_target`, run `luvus agent list` once to
   show the live choices. Never guess or retry a different target without the
   user's choice.
4. For any other error, report it without listing agents. Never absorb or
   perform the delegated task locally after delivery fails. Start an agent only
   when the user clearly requested it.

Plain-language delegation requests follow the same workflow. Never delegate
merely because another agent could help.

## Select exactly one session

Never run bare `luvus` because it launches or attaches the TUI. Examples below
say `luvus` for readability. Substitute the client selected here.

### Inside a Luvus pane

When `LUVUS_ENV=1`, control the inherited session:

- Require `LUVUS_BIN_PATH`, `LUVUS_SOCKET_PATH`, and `LUVUS_PANE_ID`.
- Invoke the exact `LUVUS_BIN_PATH`. Let it use the inherited socket.
- Keep `LUVUS_PANE_ID` as the caller and default split anchor.
- Never replace the inherited socket or binary with a default, a PATH lookup,
  or another Luvus session.

Act on the inherited session only because the user is already inside that
session and explicitly asked for delegation or control.

### Outside Luvus

Use the installed production Luvus client:

- Resolve `luvus` once with the current shell's command lookup, such as
  `command -v luvus` on Unix or `(Get-Command luvus).Source` in PowerShell.
- Invoke that exact resolved path for the rest of the request. This supports
  Luvus installed by Cargo, Homebrew, `install.sh`, or another PATH-managed
  installation.
- Preserve an explicitly configured `LUVUS_HOME` or `LUVUS_SOCKET_PATH`.
  Preserve `LUVUS_SESSION` too. When the user explicitly names a server
  session, pass `--session <name>` directly to every related Luvus command.
  Do not list sessions first, and do not silently fall back to `default`.
  Otherwise let the installed release binary use its production default at
  `$HOME/.luvus/luvus.sock`.
- Never substitute `LUVUS_BIN_PATH` or a repository build for a missing
  installed command.
- Only after an attempted Luvus action cannot run because command lookup finds
  no `luvus` client, report that Luvus is not installed and stop. Offer one of
  the supported commands: `curl -fsSL https://luvus.dev/install.sh | sh`,
  `brew install RizRiyz/luvus/luvus`, or `cargo install luvus`. Do not show this
  guidance preemptively or for socket, permission, server, or other command
  failures. Do not imply Luvus is available until a later lookup succeeds.
- Start the production server only when the user asked to start or use Luvus
  and starting it is a normal required step.

In the Codex app, local socket access may require permission outside the
workspace sandbox. Request that permission for the selected Luvus client. Do
not describe an unapproved sandbox failure as an offline server. Only report
the production server offline after an approved command cannot connect.

Use the same selected binary and socket for the entire request.

### Manage named server sessions

A named session is an independent Luvus server and PTY tree, not a workspace.
Use these commands only when the user explicitly asks to inspect or manage
server sessions:

```sh
luvus session list
luvus session attach <name>
luvus session stop <name>
luvus session delete <name>
luvus --session <name> pane list
```

`session attach` launches or attaches the TUI. Never run it merely to test
whether a session exists. `session stop` ends every pane in that named server.
Before deletion, list sessions once, require the exact stopped name, and obtain
clear authorization. Never delete `default` and never substitute workspace
commands for server-session commands.

## Use the fast command path

- Run the requested semantic command directly. Do not prepend routine `help`,
  `ping`, status, or list calls.
- For delegation, call `agent send` directly. Treat `agent list` as recovery
  only after `not_found` or `ambiguous_target`.
- Use `help` only when syntax is uncertain or a command is unsupported.
- Reuse live IDs, names, and results already returned in the thread.
- Trust a successful mutation response that identifies its target. Do not
  immediately reread the same state unless the response is incomplete.
- Prefer one broad list over one status call per item.
- Use bounded Luvus waits instead of shell sleeps or polling loops.

Most commands return JSON with `.result` or `.error`. Parse exact IDs, indices,
paths, names, and statuses from those results. Never infer a target from sidebar
position.

## Choose the CLI or UHP deliberately

Use the semantic CLI for ordinary Luvus control and delegation. It is the
shortest path for one action and should not be replaced with raw protocol or
terminal input.

Use Universal Harness Protocol 1.0 only when the user explicitly asks for a
Luvus API, harness integration, capability or schema discovery, sequenced
events, fenced snapshots, revision-safe automation, delegated access, or
terminal-backend streaming. For that work, discover the installed server
instead of assuming support from documentation or a release number:

```sh
luvus uhp capabilities
luvus uhp schema
luvus uhp snapshot
```

These discovery calls are an exception to the no-preflight rule because they
define the live protocol contract. Do not run them before routine CLI actions.
`luvus uhp proxy` forwards one newline-delimited JSON request from stdin to the
selected local server. Validate the method and parameters against the installed
schema before sending it.

`luvus uhp access` is the transport-neutral foreground boundary for an
explicit remote provider or client. It emits one machine-readable descriptor
for an authenticated loopback endpoint and creates no public listener. Use
`--control` only when the user explicitly authorizes the documented limited
control surface. Keep the command running only for the requested access
window, and never expose its loopback endpoint without a trusted encrypted
transport.

For stateful UHP automation:

1. Read capabilities and limits.
2. Subscribe from a known event sequence and obtain a fenced snapshot.
3. Apply only later events in order.
4. Resnapshot after a sequence gap, overflow, reconnect, server-generation
   change, or `resync_required` event.
5. Use advertised revisions or `if_revision` for conflicting mutations.
6. Prefer atomic methods such as `agent.start`, `agent.prompt`, `layout.apply`,
   `workspace.move_block`, and `diff.note.apply` when the live capabilities
   advertise them.

The default endpoint trusts the local owner and is not a public TCP service.
Create scoped, expiring UHP tokens only for an explicitly delegated harness.
Never print, persist, or log token secrets, and never grant broader scopes than
the requested integration requires. Terminal observe and control streams are
for explicit harness or remote-client work. Control is exclusive, bounded, and
must be released when the task ends. Prefer `agent prompt`, `pane read`, and
other semantic routes for ordinary automation.

For method families, event recovery, tokens, revisions, layout operations, and
terminal streams, read
[uhp-control.md](references/uhp-control.md) when it is installed. The rules
above remain sufficient when `luvus skill show` is the only available file.

## Delegate and manage agents

Resolve active agents with:

```sh
luvus agent list
luvus agent get <target>
```

Targets are a live name, pane id, or unique agent kind. If a kind is ambiguous,
ask the user to choose or name the panes. Never guess.

To start and name a sibling agent from a managed pane:

```sh
luvus agent start reviewer --kind codex --timeout 30
```

From an external terminal, select a live anchor first:

```sh
luvus agent start reviewer --kind codex --anchor <pane-id> --timeout 30
```

Try the anchored start directly. Only when it returns a syntax or usage error
showing that `--anchor` is unsupported, use the compatible two-command path and
pass the new pane id returned by `pane split`. Do not run a help preflight:

```sh
luvus pane split <anchor-pane-id> --no-focus
luvus agent start reviewer --kind codex --pane <new-pane-id> --timeout 30
```

Omit `--down` for a right-side split and add it to the split or anchored start
for a split below. Never combine `--anchor` and `--pane`.

Send work with `agent send`, not raw pane text and Enter:

```sh
luvus agent send reviewer "Review the current diff"
```

Do not add `--wait` unless the user explicitly asks to wait or the next required
step depends on the result. In a managed pane, name the caller and ask the
worker to report back when asynchronous delivery is useful:

```sh
luvus agent name lead
luvus agent send reviewer "Review the diff. When done, run: luvus agent send lead 'done: <summary>'"
```

After a no-wait handoff, end the turn. The report-back message starts a fresh
turn. An external terminal has no caller pane, so do not invent one.

When waiting was requested, keep it bounded and read a bounded result:

```sh
luvus agent send reviewer "Review the diff" --wait --timeout 300
luvus agent read reviewer --lines 120
```

Treat `idle`, `done`, `working`, and `blocked` as ready once the requested agent
identity is recognized. `unknown` is not proof of completion, but it does not
undo a matching identity. When `agent start` returns `ready: true`, accept its
name, pane, and kind without another status lookup. Use `wait agent-status` for
a requested lifecycle transition after work is sent, not for startup identity.
Repeat `--status` or pass a comma-separated set when any of several terminal
states should unblock the workflow. Unknown options and positional arguments
are rejected instead of being ignored.

For a blocked agent:

1. Run `luvus agent get <target>`.
2. Run `luvus agent read <target> --source visible --lines 120`.
3. Identify the exact approval or question.
4. Send `agent keys` only when the user's request authorizes that effect.

`agent keys` accepts only a recognized agent pane and a non-empty list of known
key names. It validates the entire list before queuing one ordered action; any
invalid entry sends nothing, and a closed target returns `send_failed`.

## Control panes, tabs, and workspaces

Use these read routes before a write whose target is not already known:

- `luvus workspace list`
- `luvus tab list`
- `luvus pane list`
- `luvus pane status <id>`
- `luvus pane read <id> --lines 120`
- `luvus search <query>`

When the user supplies a stable 0-based workspace index, run the requested
mutation directly. Run `workspace list` only to resolve a name, path, or sidebar
position to an index, or once after `not_found` to show current choices. Reuse a
current list result from the thread and do not run `help` before these documented
commands. Renaming changes the label, never the folder; pinning changes sidebar
display order, never the API index:

```sh
luvus workspace rename <workspace-index> <name>
luvus workspace pin <workspace-index>
luvus workspace unpin <workspace-index>
```

Closing the final project does not stop the client or leave it without a shell.
Luvus immediately creates a neutral workspace and terminal at the user's home
directory. Its displayed path follows the focused pane's live cwd. An empty
`workspace list` is therefore an exceptional restore or spawn failure, not proof
that the server is offline. Use `workspace open <path>` when the user named a
specific project.

From a managed pane, use `LUVUS_PANE_ID` as the caller or split anchor. From an
external terminal, select an explicit pane returned by live state.

To run an ordinary command beside an explicit pane without stealing focus:

```sh
luvus pane split <anchor-pane-id> --no-focus
luvus pane run <new-pane-id> "cargo test"
luvus wait output <new-pane-id> --match "test result" --timeout 300
luvus pane read <new-pane-id> --lines 120
```

Fork a supported live agent only after resolving it with `agent get`. The fork
inherits the source conversation but receives its own new session:

```sh
luvus agent get <target>
luvus agent fork <target> [--name <alias>] [--no-focus]
```

Native forks currently support Claude, Grok, Codex, Pi, and OMP. Report
`unsupported_agent`, `session_unknown`, or `spawn_failed` exactly when returned.
Do not approximate a failed fork with `pane split`, `agent start`, or `resume`,
because those paths do not guarantee an independent copy of the conversation.

Move an existing pane only after resolving its id and listing the destination
tabs in that pane's workspace. Tab numbers are 1-based:

```sh
luvus pane move <pane-id> --tab <tab-number>
luvus pane move <pane-id> --new-tab
```

Reorder tabs in the active workspace only after `luvus tab list` confirms the
source and final positions:

```sh
luvus tab move <from> <to>
```

## Control advanced surfaces safely

This section remains complete when `SKILL.md` is installed by itself. If
[advanced-control.md](references/advanced-control.md) is available, read it for
a compact command index. Its absence is not a blocker and never permits a
guess or a weaker safety check.

Use these read routes to resolve state and exact targets:

- Files and Git: `luvus files tree`, `luvus git status`,
  `luvus git branches`, `luvus git log`
- DIFF: `luvus diff list`, `luvus diff get <path>`,
  `luvus diff note list`
- Worktrees: `luvus worktree list`
- Orchestration: `luvus task list`, `luvus task get <id>`,
  `luvus lease list`
- Modules: `luvus module list`, `luvus module info <id>`,
  `luvus module actions`, `luvus module settings <id>`,
  `luvus module log <id>`
- Themes and UI: `luvus theme list`, `luvus bar list`,
  `luvus ui dock list`

Run `luvus help all` only when the requested mutation grammar is uncertain.
This remains compatible with older Luvus releases. Before changing an advanced
surface:

- Inspect files and Git before opening a file, revealing a path, refreshing
  the tree, or opening a Git view.
- Inspect the exact DIFF layer and file before opening it or adding, editing,
  resolving, removing, applying, or sending a review note. Removing a note and
  sending feedback to an agent require explicit authorization.
- List worktrees before creating, opening, or removing one. Removal requires
  explicit authorization and an exact path.
- Inspect task and lease ownership, dependencies, gates, assignees, and path
  leases before claiming, starting, updating, completing, releasing, deleting,
  or merging. `task merge` is serialized into
  `luvus/integration`: wait for its response, treat `merged` as terminal, and
  resolve a reported conflict before retrying. A branch-backed dependency does
  not unblock its dependents until it is merged. `task start` defaults to
  isolated `mode=worktree`; explicit `mode=workspace` creates a branchless task
  tab in a shared checkout and cannot be merged. Start checks leases before
  creating either worker, so resolve a returned `lease_conflict` before
  retrying. Leases coordinate declared paths but do not sandbox a shared
  checkout. `task release` requeues an
  active task and releases its path leases; it does not stop the worker pane.
  Report work progress only with `task update --note`. `task heartbeat
  --context-used <0..1>` means the fraction of the model context window already
  consumed, never task-completion progress; `0.6` means 60% consumed. Omit the
  heartbeat when context-window usage is unknown.
- Inspect `automation.list`, `automation.get`, and `automation.history` before
  changing an agent schedule. Creating, updating, enabling, disabling,
  deleting, or manually running an automation requires explicit authorization.
  Preview calendar triggers first, retain the user's IANA timezone, use an
  idempotency key for retryable create/run requests, and never turn an
  automation into an arbitrary scheduled shell command. Disabling prevents
  future occurrences; it does not stop a live ORCH task or pane.
  Mutation success waits for the ledger checkpoint. After `persistence_failed`
  or a lost response, inspect state and retry the same idempotency key; do not
  assume the definition is absent or create a duplicate schedule. A failed
  prelaunch occurrence is not automatically relaunched on storage recovery.
  `target=new_worker` is the durable default. Use `active_agent` only with
  the exact discovered pane, terminal lifetime, agent, and workspace identities;
  treat `delivered` as input-queue evidence rather than completed work, and do
  not reuse a process-bound target after pane closure or server restart. A
  target advertised as `binding=durable` may recover only the same private
  native conversation. When it reports `needs_rebind`, inspect the intended
  pane first and use `automation.rebind`; never substitute another session.
- Inspect module metadata, actions, settings, and logs before changing module
  state. Installation, uninstallation, and consequential setting changes need
  clear authorization.
- Validate theme sources before installing them. Do not uninstall the active
  theme or fetch a remote theme without explicit authorization.
- CLI widget commands use `luvus bar ...`; the UHP method family is
  `ui.bar.*`. Inspect widgets and docks before changing placement or content.
  Avoid sidebar, dock, notification, toast, bar, or focus changes unless they
  serve the user's request.
- Open Mission Control directly with `luvus mission open [<workspace>]` when
  the user asks for it. The optional workspace index is 0-based; omit it to
  target the active workspace. For UHP automation, use `mission.snapshot` with
  workspace scope (the default) or explicit all-workspace scope to inspect
  data without changing the UI,
  `mission.refresh` for an explicit usage refresh, and `mission.open` only to
  change the visible tab.
- Agent detection is built into Luvus. `luvus integration install` manages
  optional native session-resume hooks and must not be used merely to make an
  agent appear in the sidebar. Install or remove an integration only when the
  user explicitly requests that lifecycle integration.
- For Antigravity CLI, `luvus integration install antigravity` adds exact
  conversation identity for restore. It is session-only; native screen
  detection remains authoritative for agent state.
- For OpenCode, `luvus integration install opencode` adds exact TUI-local root
  session ownership and structured usage. Without it, usage stays unavailable.
- For Hermes, `luvus integration install hermes` adds exact per-pane session
  ownership for restart resume. Detection remains native, but Luvus does not
  scan Hermes's private history store.
- Subscribe to events only for a live monitoring request. Stop when its
  condition is satisfied and never retain an unbounded stream.

## Learn configuration and unfamiliar surfaces

Use `luvus help <topic>` for the installed command grammar and
`luvus uhp capabilities` plus `luvus uhp schema` for a selected server's live
automation contract. For configuration, installation, concepts, or a surface
not covered here, read https://luvus.dev/agent-readme.md first and use
https://luvus.dev/llms.txt to open only the relevant documentation page.

Luvus preferences live in `config.json` under `~/.luvus/`, or under the
explicit `LUVUS_HOME`. Debug builds use `~/.luvus-dev/`. Prefer the Settings
screen for validated live changes, preserve unknown fields during a manual
edit, and never substitute production, development, or named-session paths.
The installed binary and selected running server remain authoritative when the
website describes a newer release.

## Safety

- Use explicit targets for writes. A focused pane may belong to another client.
- Preserve focus and inactive-pane scroll positions unless asked to change
  them.
- Preserve prompts, paths, Unicode, quotes, dollar signs, equals signs, and
  newlines as arguments. Avoid an unnecessary `sh -c` interpolation layer.
- Do not close panes, tabs, or workspaces, remove worktrees, delete or merge
  tasks, uninstall modules, or overwrite consequential settings without clear
  authorization and a read-only target check.
- Never stop or restart a Luvus server as a normal control step. Do so only
  after an explicit request and a warning that every managed pane is affected.
