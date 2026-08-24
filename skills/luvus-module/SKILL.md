---
name: luvus-module
description: >-
  Write a luvus module (an extension for luvus, mission control for your AI coding agents). Use when
  the user is building or debugging a luvus module: authoring luvus-module.toml,
  adding a sidebar dock, Luvus Bar widget, right-click action, event hook,
  module pane, module settings, or calling luvus back over its UHP.
---

# Writing a luvus module

A **luvus module** is a plugin for luvus, mission control for your AI coding agents. There is **no SDK and no scripting engine**: a module is a directory with a `luvus-module.toml` manifest that declares **argv commands** (any executable: `sh`, `python`, `node`, a compiled binary). luvus runs those commands as subprocesses with `LUVUS_*` context in the environment, and the command calls luvus back through the same UHP the `luvus` CLI uses. So a module in bash/python/node can do what a built-in feature does.

Reference material in this repo (read for depth): `MODULE-GUIDE.md`, `docs/13-modules.md`, and the worked modules in `examples/modules/` (`branch-dock` = a sidebar dock, `agent-ping` = an event hook, `scratch-pane` = a pane).

## The manifest: `luvus-module.toml`

Top-level keys (all required unless noted):

```toml
id = "yourname.my-module"     # unique, reverse-dns style
name = "My Module"
version = "0.1.0"
min_luvus_version = "0.8.3"   # oldest luvus this needs
description = "One line."      # optional
platforms = ["macos", "linux"]  # optional; omit = all. Also per-item.
```

Then any of these tables, each declaring an argv `command` (a list, run as-is, cwd = the module dir):

- **`[[docks]]`** `id`, `title`, `placement` (`sidebar.left` | `sidebar.right`) — reserve a sidebar dock. luvus renders it; you fill it with `ui.dock.push` (see below).
- **`[[bars]]`** `id`, `title`, `region` (`top-right` | `bottom-right`), optional `priority` — reserve a bounded one-row Luvus Bar widget. Publish structured segments with `luvus bar push` (`ui.bar.push` on the UHP); live content is not persisted.
- **`[[startup]]`** `command` — run once when the session is up and the socket is listening (and on enable). Dock rows and bar content are **not** persisted, so this is how you repaint them after a restart.
- **`[[events]]`** `on`, `command` — run when a luvus event fires. Valid `on` values include `workspace.created`/`closed`, `tab.created`/`closed`/`moved`, `pane.created`/`closed`/`forked`/`moved`, `pane.agent_status_changed`, `agent.hook`, and the `task.*`/`lease.*` events (see `KNOWN_EVENTS` in `src/module/manifest.rs` for the full set — an unknown `on` is a hard manifest error).
- **`[[actions]]`** `id`, `title`, `command`, optional `contexts` — a runnable action. With `contexts = ["pane"|"workspace"|"node"|"agent"|"tab"]` it also appears in that right-click menu, acting on **what was clicked**. Without `contexts` it is CLI-only (`luvus module run <id> <action>`). Dock rows also invoke an action on click.
- **`[[panes]]`** `id`, `title`, `command`, `placement` (`split` | `overlay` | `tab`) — a real pane running your command (`luvus module pane open <id> <entrypoint>`).
- **`[[settings]]`** `key`, `title`, `type` (`bool` | `string` | `number` | `enum`), plus `default`, `options` (enum), `min`/`max`/`step` (number), `secret` (mask + hide the value). Rendered in Settings → Modules; values reach every command as env (below).

## What your command receives (no JSON parsing needed)

luvus puts context in the environment, flat, so a bash module never parses JSON:

- `LUVUS_MODULE_ID`, `LUVUS_MODULE_ROOT` (the module dir), `LUVUS_MODULE_VERSION`
- `LUVUS_MODULE_CONFIG_DIR`, `LUVUS_MODULE_STATE_DIR` — writable per-module dirs
- `LUVUS_WORKSPACE_ID`, `LUVUS_WORKSPACE_CWD`, `LUVUS_TAB_INDEX`
- `LUVUS_PANE_ID`, `LUVUS_PANE_CWD`, `LUVUS_PANE_AGENT`, `LUVUS_PANE_STATUS` (the clicked/target pane)
- `LUVUS_SETTING_<KEY>` for each declared setting (uppercased key), plus the whole set as JSON
- Dock-row clicks add `LUVUS_MODULE_DOCK_ID`, `LUVUS_MODULE_ROW_ACTION`, `LUVUS_MODULE_ROW_VALUE`, `LUVUS_MODULE_ROW_TEXT`, `LUVUS_MODULE_ROW_INDEX`
- Bar-segment clicks add `LUVUS_MODULE_BAR_ID`, `LUVUS_MODULE_BAR_SEGMENT`, and optional `LUVUS_MODULE_BAR_VALUE`
- `LUVUS_MODULE_CONTEXT_JSON` — the full snapshot, if you want structured data
- `LUVUS_SOCKET_PATH`, `LUVUS_BIN_PATH` — to call luvus back (below)

## What your command can do: call luvus back

Run the `luvus` CLI from inside the command; it talks to the running server over `$LUVUS_SOCKET_PATH`. Use `"$LUVUS_BIN_PATH"` to guarantee the same binary as the session. Module-facing methods:

- `luvus ui dock push --id <dock> --rows <json>` (or pipe the JSON on stdin) — fill your dock. Rows are `{text, action?, value?}`; a row's `action` invokes one of your `[[actions]]` on click, with the row's `value` in `LUVUS_MODULE_ROW_VALUE`.
- `luvus bar push --id <widget> --content <json>` — atomically publish structured `text`, `symbol`, `state`, `badge`, `progress`, `spacer`, and `separator` segments. Add `--compact-content`; an `action` must name one of your `[[actions]]`.
- `luvus ui notification push --text <text> --level info|success|warning|error` — publish bounded transient status; use `--dedupe-key` for replacement.
- `luvus ui toast "<text>"` — flash a one-line confirmation.
- `luvus ui sidebar` / `ui dock list` / `ui dock move` — sidebar/dock control.
- `luvus tab rename <name>` / `tab list` — tabs.
- `luvus module settings <id>` / `module settings <id> <key>` — read your settings exactly (values are masked in `settings list` when `secret`, but exact in `settings get`).
- Plus the whole CLI: `luvus pane split/run/send`, `luvus agent ...`, `luvus git ...`, etc.

## Recipes

**A sidebar dock** (like `examples/modules/branch-dock`): declare `[[docks]]`, a `[[startup]]` that runs a script which builds a JSON rows array and calls `ui dock push`, and `[[events]]` so it refreshes on `workspace.created`/`tab.created`. Give rows an `action` matching a `[[actions]]` id to make them clickable.

**A Luvus Bar widget** (like `examples/modules/ci-bar`): declare `[[bars]]`, publish full and compact structured content from a one-shot startup/event action, and keep expensive work outside the render path. Use a pane for arbitrary terminal UI and a dock for multi-row content.

**An event hook** (like `agent-ping`): a `[[events]]` with `on = "pane.agent_status_changed"` and a command that reads `LUVUS_PANE_*` and reacts (notify, log, `ui toast`).

**A right-click action**: an `[[actions]]` with `contexts = ["pane"]` (or workspace/node/agent/tab). It appears in that menu and runs against the clicked target's `LUVUS_*` env.

**A pane**: a `[[panes]]` entry; open it with `luvus module pane open <module-id> <entrypoint>`.

## Test loop

```
luvus module link <path-to-module-dir>     # register a local module (dev)
luvus module list                          # confirm it's runnable
luvus module log [<id>]                     # tail command output/errors while iterating
luvus module run <id> <action>              # invoke an action directly
luvus module enable <id> | disable <id>
```

Iterate: edit the script, re-run the action or trigger the event, watch `module log`. The manifest is validated on link; a broken manifest keeps the entry visible but not runnable, with the reason in `module info <id>`.

## Conventions

- One module, one job. Keep commands fast and quiet — luvus caps a command's output (64 KiB) and runs at most 32 at a time.
- Identity env (`LUVUS_MODULE_ID`, socket path) is injected and cannot be overridden.
- Use `platforms` on the manifest or any item to skip a build step / hook / pane / action where it does not apply.
- To share it: `luvus module install <owner>/<repo>` clones + builds + registers from GitHub; tag the repo with the `luvus-module` topic so `luvus module search` finds it.
