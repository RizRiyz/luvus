# Example modules

Six complete, working luvus modules across three languages, each covering a
different part of the extension surface. They are meant to be **copied and
edited**, not installed as-is.

| Module | Language | Covers |
|---|---|---|
| [`branch-dock`](branch-dock) | Bash | A sidebar **dock**, clickable rows with a `value` payload, a **startup hook**, `number` + `enum` **settings**, a `workspace` right-click action |
| [`agent-ping`](agent-ping) | Python | An **event hook** on agent status, a **secret** setting, an `agent` right-click action, toasts |
| [`scratch-pane`](scratch-pane) | Node | A **pane** entrypoint, `pane` right-click actions, reading the **selection**, **renaming a tab**, the state dir |
| [`file-tree`](file-tree) | Bash | A **collapsible file tree** dock (per-row `toggle`/`open` actions, on-disk expand state), opening a file into a split **pane** via `pane split` + `pane run` — a no-core-edits prototype of docs/38 |
| [`ci-bar`](ci-bar) | Bash | A multi-segment **Luvus Bar** widget, compact content, a clickable action, startup restoration, and a transient notification |
| [`telegram-notify`](telegram-notify) | Bash | An **event hook** on agent status that sends a **Telegram** message (`blocked`/`done`/`both`), a **secret** setting, an `agent` right-click test-send action, redacted failure logging via `luvus module log` |

Nothing here needs a build step or a dependency beyond the language runtime
itself (`sh`, `python3`, `node`).

## Try one

```sh
luvus module link ./examples/modules/branch-dock
luvus module list
```

`branch-dock` paints its dock immediately (its startup hook runs on link), so
you should see a **BRANCHES** section appear in the left sidebar when the
active node is a git repo. Click a branch to check it out. Open
**Settings → Modules** to see its two settings, and right-click a WORKSPACES row
for its "Refresh branches" entry.

Remove it again with:

```sh
luvus module unlink example.branch-dock
```

## Reading them

Start with `branch-dock/refresh.sh`. It is the shortest demonstration of the
whole idea: read the injected `LUVUS_*` variables, do some work, and call back
through `$LUVUS_BIN_PATH`. There is no SDK to import in any of these files.

## Writing your own

Full reference: **[luvus.dev/docs/extend/writing-modules](https://luvus.dev/docs/extend/writing-modules/)**.

Two rules worth knowing up front:

- Call back through `$LUVUS_BIN_PATH`, never a bare `luvus` on `PATH`. It points
  at the running binary, so your module works across Unix sockets and Windows
  named pipes.
- Write durable data to `$LUVUS_MODULE_STATE_DIR` or `$LUVUS_MODULE_CONFIG_DIR`,
  never into the module directory. For a git-installed module that directory is
  a managed checkout a reinstall replaces.
