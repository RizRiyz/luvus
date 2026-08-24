# File Tree (prototype module)

A collapsible file tree of the active node in the sidebar, and a file opener that
runs a pager in a pane. Built entirely as a **module** — no changes to luvus
core — to validate the UX of docs/38 fast.

```sh
luvus module link examples/modules/file-tree
```

A **FILES** dock appears in the left sidebar (move it in Settings → Layout).

- **Click a folder** to expand or collapse it. Only expanded folders are read, so
  a large repo costs what you open, not the whole tree.
- **Click a file** to open it in a pager pane (`bat` if installed, else `less -N`).
- **Click the `⟳` header row** to re-root the tree on the node you are on now.
- **Right-click a WORKSPACES row → "Open file tree here"** does the same.

Settings (Settings → Modules → File Tree):

- **Open files in** — `pane` (split beside you) or `tab` (a new tab).
- **Show dotfiles** — include entries beginning with `.` (`.git` is always hidden).
- **Max rows** — cap the tree so a huge expanded repo can't push an enormous dock.

## What this prototype proves, and where it stops

This is the fast-validation path from docs/38. It is a real, working file browser
with **zero core edits**, and it deliberately stops where a module must:

- The tree is a **flat pushed list** (indentation is text). Every expand/collapse
  re-pushes the whole dock, so it is fine for browsing but not as smooth as a
  native tree on a very large repo.
- A file opens in a **pager PTY**, not a native luvus view — no theme-aware
  renderer, no shared scroll, no in-file search. A module can only open real PTY
  panes; the native view pane (docs/38 FILE-3, shared with docs/30) is the
  upgrade that makes "open a file" first-class.

If the UX feels right here, docs/38 is the plan to make it native.

## How it works (no SDK, just argv + the socket)

`luvus-module.toml` reserves the dock and declares the actions. The scripts fill
the dock over the same UHP the CLI uses:

- `render.sh` — walks the expanded tree (explicit-stack DFS, since POSIX sh has
  no locals) and `luvus ui dock push`es it. Folders get the `toggle` action,
  files the `open` action.
- `toggle.sh` — a clicked folder's path arrives in `LUVUS_MODULE_ROW_VALUE`; flip
  it in the expanded set (kept in `LUVUS_MODULE_STATE_DIR`) and repaint.
- `open.sh` — a clicked file: `luvus pane split` (or `tab new`), capture the new
  pane id, `luvus pane run <id> <pager>`.
- `lib.sh` — shared state helpers and JSON escaping.
