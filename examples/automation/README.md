# Agent Automation example

`weekday-readonly-review.sh` creates a weekday read-only review for an existing
Luvus workspace. It previews the next occurrences before storing anything and
uses an idempotency key so retrying the same command cannot create a duplicate.

Find the stable workspace ID first:

```sh
luvus workspace list
```

Then run the example with that ID and an IANA timezone:

```sh
./examples/automation/weekday-readonly-review.sh \
  workspace_example \
  Asia/Makassar
```

For a named session, pass its name as the third argument:

```sh
./examples/automation/weekday-readonly-review.sh \
  workspace_example \
  Asia/Makassar \
  work
```

The fourth argument can override the idempotency key. Reuse a key only when the
complete create request is identical. Use a new key when changing the schedule,
agent, prompt, workspace, or policy.

Inspect or stop the resulting definition with:

```sh
luvus automation list
luvus automation health
luvus automation history <automation-id> --limit 20
luvus automation disable <automation-id>
luvus automation delete <automation-id>
```

Add `--session <name>` before `automation` for every command that targets a
named session. Deleting is rejected while the definition owns a live run.

To continue an agent that is already running, use `luvus agent list` to obtain
its pane, terminal, agent, and workspace IDs, then add
`--target active-agent --pane <id> --terminal-id <id> --if-busy wait` to a
create or update command. Active-agent schedules are bound to that exact PTY
lifetime and are disabled after pane closure or server restart.

The complete guide is at <https://luvus.dev/docs/guides/automation/>.
