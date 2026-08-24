# Writing a luvus module

**This guide has moved to the documentation site:**

→ **[Writing a Module](https://luvus.dev/docs/extend/writing-modules/)** —
the complete guide: manifest reference, right-click menus, settings, docks,
panes, environment variables, the context blob, calling back into luvus,
distribution, and troubleshooting.

**Building with an AI agent?** Point your coding agent at the module-authoring
skill and it will know the manifest and UHP: [`skills/luvus-module/SKILL.md`](skills/luvus-module/SKILL.md).
Install it with:

```sh
mkdir -p ~/.claude/skills/luvus-module
curl -fsSL https://raw.githubusercontent.com/RizRiyz/luvus/main/skills/luvus-module/SKILL.md \
  -o ~/.claude/skills/luvus-module/SKILL.md
```

Quick taste — a module is a directory with a `luvus-module.toml` manifest
declaring argv commands, in any language, no SDK:

```toml
id = "you.hello"
name = "Hello"
version = "0.1.0"
min_luvus_version = "0.8.3"

[[actions]]
id = "greet"
title = "Say hello"
contexts = ["pane"]          # also offer it on right-click inside a pane
command = ["sh", "greet.sh"]

[[settings]]
key = "who"                  # shows up in Settings → Modules
title = "Greet who?"
type = "string"
default = "world"
```

Luvus 0.11.x can still load a legacy `bohay-module.toml` with
`min_bohay_version`, but new modules should use the canonical Luvus names above.

```sh
#!/bin/sh
# greet.sh — everything arrives in the environment, no JSON parsing needed
"$LUVUS_BIN_PATH" ui toast "hello $LUVUS_SETTING_WHO from $LUVUS_WORKSPACE_CWD"
```

```sh
luvus module link .              # register it
luvus module run you.hello greet
luvus module log                 # status + captured output
```

A module can reach docks, panes, tabs, right-click menus, settings, lifecycle
events, and a startup hook. Anything in `luvus help all` is available to it.

## Worked examples

Three complete modules live in [`examples/modules/`](examples/modules), one per
language:

| Example | Language | Shows |
|---|---|---|
| [`branch-dock`](examples/modules/branch-dock) | Bash | A sidebar dock, clickable rows, a startup hook, number + enum settings |
| [`agent-ping`](examples/modules/agent-ping) | Python | An event hook, a secret setting, an agent right-click action |
| [`scratch-pane`](examples/modules/scratch-pane) | Node | A pane entrypoint, pane right-click actions, the selection, tab renaming |

Copy one, change the `id`, and `luvus module link` it.

See also [Using Modules](https://luvus.dev/docs/extend/using-modules/)
for discovering and installing community modules.
