# Advanced Luvus command index

This reference is optional. `SKILL.md` contains the complete targeting and
safety contract for single-file installations. Use this file as a compact
command index when it is installed by a Luvus release or the Codex plugin. If
it conflicts with `SKILL.md`, follow `SKILL.md`.

## Read routes

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
- Layout: `luvus workspace list`, `luvus tab list`, `luvus pane list`
- UHP: `luvus uhp capabilities`, `luvus uhp schema`,
  `luvus uhp snapshot`

Run `luvus help all` only when the requested command grammar is uncertain. This
remains compatible with older Luvus releases.

## Mutation checklist

- Inspect files and Git before opening a file, revealing a path, refreshing the
  tree, or opening a Git view.
- Inspect the exact DIFF layer and file before changing or sending review notes.
  Removing notes and messaging an agent require explicit authorization.
- List worktrees before creating, opening, or removing one. Removal requires
  explicit authorization and an exact path.
- Inspect task and lease ownership, dependencies, gates, assignees, and path
  leases before claiming, starting, updating, completing, releasing, deleting,
  or merging.
- Inspect module metadata, actions, settings, and logs before changing module
  state. Installation, uninstallation, and consequential setting changes need
  clear authorization.
- Validate theme sources before installation. CLI widgets use `luvus bar`; UHP
  widgets use `ui.bar.*`. Inspect current placement before moving or removing a
  widget or dock.
- Inspect docks before moving them. Avoid sidebar, dock, toast, or focus changes
  unless they serve the user's request.
- Open Mission Control directly when the user asks for it. The optional
  workspace index is 0-based; omit it to target the active workspace.
- With an explicit stable index, run `luvus workspace rename <i> <name>`, `pin
  <i>`, or `unpin <i>` directly. List workspaces only to resolve an unknown
  target or recover from `not_found`, and reuse a current list result. Workspace
  indices and returned `display_position` values are 0-based; pinning changes
  only display order.
- Resolve a live agent with `luvus agent get <target>` before `luvus agent fork
  <target> [--name <alias>] [--no-focus]`; the fork creates a new independent
  session and may change focus unless `--no-focus` is passed.
- Resolve a pane and list its workspace's tabs before `luvus pane move <id>
  --tab <n>` or `--new-tab`. List the active workspace's tabs before `luvus tab
  move <from> <to>`; tab positions are 1-based.
- Named servers are selected directly with `luvus --session <name> ...`.
  `session list` is discovery only, `session attach <name>` opens the TUI,
  `session stop <name>` ends only that server, and `session delete <name>` is
  allowed only after an exact stopped target and explicit authorization.
- Subscribe to events only for a live monitoring request. Stop when its
  condition is satisfied and never retain an unbounded stream.
- For explicit harness or protocol work, read
  [uhp-control.md](uhp-control.md), discover the live capabilities and schema,
  and resnapshot after event loss or a server-generation change.
- Agent detection is independent of optional resume integrations. Never install
  an integration merely to make an agent visible.
- Do not remove worktrees, delete or merge tasks, uninstall modules, or
  overwrite consequential settings without clear authorization and a read-only
  target check.
- Stop or restart the Luvus server only after an explicit request and a warning
  that it ends every managed pane.
