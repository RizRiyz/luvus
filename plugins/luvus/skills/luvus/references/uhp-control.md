# UHP control reference

Read this reference only for explicit Luvus API, harness, protocol, event,
revision, token, layout, or terminal-backend work. Use the semantic `luvus`
commands in `SKILL.md` for ordinary one-shot control.

## Discover the installed contract

Treat the selected running server as authoritative:

```sh
luvus uhp capabilities
luvus uhp schema
luvus uhp snapshot
```

Capabilities report the protocol version, method contracts, access mode,
required scope, idempotence, atomic methods, limits, event sequence, terminal
features, and server identity rules. The schema defines exact request, response,
and general event payload fields. Do not infer a method from a newer website or
binary.

`luvus uhp proxy` accepts one newline-delimited request from stdin and emits one
response. A request has `id`, `method`, and `params`; add `auth` only for an
explicit delegated token. Keep the inherited or explicitly selected session
and socket when invoking it.

`host.capabilities` discovers a separate on-demand profile handled by the
proxy itself. Its host diagnostics, named-session lifecycle, update, skill,
and integration methods can work without a running server. They trust only the
local operating-system account, reject delegated tokens, and are not exposed
through `uhp access`. Every host mutation that installs or deletes data
requires `confirm:true` and the human's explicit authorization.

`luvus uhp access [--control]` instead creates a temporary authenticated
loopback gateway for a persistent third-party byte-stream provider. Its first
stdout line is the access descriptor; it never contains the delegated token.
The client pairs once, then uses the returned token on ordinary UHP frames.
Keep the loopback endpoint private and require an encrypted, authenticated
transport.

Control Access permits `agent.keys` for recognized agent panes; read-only
Access denies it. The RPC retains local key grammar, including `ctrl+z`,
printable Unicode, and `["esc","[","Z"]` for Shift+Tab, which is wider than
control-stream `send_key`. Invalid batches queue no prefix. Success identifies
the resolved `pane` and means queued, not consumed. Inspect before answering;
do not infer permission for `agent.send`, raw pane input, launch, fork, or close.

Control also permits `pane.rename` with the existing `pane` and `name` parameters;
read-only Access denies it. Rename retains the owner name validation and
`pane.renamed` event. An empty name clears the pane alias.

After pairing through Access, `uhp.capabilities` retains owner `methods` and
adds `access.mode`, `access.allowed_methods`, and gateway-specific
`access.limits.connections` / `requests_per_minute`. Intersect the allowed set
with server methods and your supported actions. Owner endpoints omit `access`;
older gateways may omit it too, which never proves write permission. Control
includes `pane.rename`, keys, and existing automation writes, but excludes standalone terminal
input and token administration. Re-discover after reconnect; accept unknown
additive fields. No owner socket/token or new event is exposed.

## Bootstrap and maintain state

Use this order for a stateful harness:

1. Read capabilities and record the current event sequence and limits.
2. Subscribe with `after_sequence` before or alongside snapshot acquisition.
3. Fetch `session.snapshot`, which fences the returned state with its sequence
   and server generation.
4. Discard buffered events at or below the snapshot sequence.
5. Apply later events in order.
6. Resnapshot after a gap, overflow, reconnect, generation change, or
   `resync_required` event.

Use `events.wait` for one bounded semantic condition. Use `events.subscribe`
only when the user asked for continuous monitoring or a harness genuinely needs
a stream. Stop the subscription when the condition or integration ends.

Workspace and tab identities are stable. Terminal identities last for one PTY
lifetime. Never translate a sidebar position into an ID. For writes, carry the
latest advertised revision and use `if_revision` when supported. On conflict,
read current state and reconcile instead of blindly retrying.

## Choose the narrowest method family

- Workspace topology: `workspace.*`, `tab.*`, `pane.*`, and `layout.*`
- Agents: `agent.*`, with `agent.prompt` preferred for atomic prompt submission
- Search, files, Git, and review: `search.*`, `files.*`, `git.*`, and `diff.*`
- Mission Control: read with `mission.snapshot`, refresh usage on demand with
  `mission.refresh`, and change the visible UI only with `mission.open`
- Worktrees and orchestration: `worktree.*`, `task.*`, and `lease.*`
- Agent scheduling: inspect with `automation.list`, `automation.get`,
  `automation.history`, `automation.preview`, and `automation.health`; mutate
  with `automation.create`, `automation.update`, `automation.enable`,
  `automation.disable`, `automation.rebind`, `automation.run`, and
  `automation.delete`
  - For create or update, set `task.access` to `read_only`, `workspace`, or
    `full_access`; omitted access defaults to `workspace`. This is independent
    of `task.mode`. Never retry an unsupported agent/access pair with broader
    access unless the user explicitly selected it.
  - Kilo Code new-worker schedules support only an explicitly selected
    `full_access` profile because its reviewed unattended command is
    `kilo run --auto`. Never broaden a Kilo schedule automatically.
  - `target` defaults to `new_worker`. Use `active_agent` only with the exact
    live `pane_id`, `terminal_id`, `task.agent_id`, and `task.workspace_id`
    returned by discovery. Its `if_busy` policy is `wait` or `skip`; it creates
    no ORCH worker and `delivered` proves queueing, not task completion. Do not
    reuse a `process_bound` target after pane closure or server restart. A
    `durable` target may be reattached with `automation.rebind` only when the
    selected pane proves the same native conversation.
- Extensions: `module.*`
- Themes and configuration: `theme.*`, `config.*`, and `manifest.reload`
- UI surfaces: `ui.sidebar`, `ui.agent_title.*`, `ui.dock.*`, `ui.bar.*`,
  `ui.notification.*`, and `ui.toast`
- Terminal backends: `terminal.backend.*`

Prefer read-only discovery before a write when the exact target, revision, or
ownership is not already known. Prefer advertised atomic methods for compound
operations. Do not reconstruct `agent.start`, `agent.prompt`, `layout.apply`,
`workspace.move_block`, or `diff.note.apply` from weaker individual actions.

CLI `luvus bar` commands map to the UHP `ui.bar.*` family. Agent detection is a
core runtime feature; integration hooks and manifest reloads are separate and
must not be used as a generic detection repair.

## Delegated authorization

The local endpoint grants the local owner full authority. Delegated UHP tokens
are optional and exist only for deliberately connected harnesses.

- Create a token only with explicit authorization.
- Grant the smallest required scopes and a bounded expiry.
- Never grant a scope the caller does not hold.
- Never print, persist, commit, or log the returned secret.
- List token metadata without exposing secrets.
- Revoke the token when the integration ends or access is uncertain.

Available scope families are discovered live and can include `read`,
`workspace`, `agent`, `terminal`, `orchestration`, `extensions`, `admin`, and
`all`. Do not use `admin` or `all` when a narrower scope works.

## Terminal observation and control

Use `terminal.backend.observe` only for an explicit terminal-rendering or remote
client. Use `terminal.backend.control` only when bidirectional control is
required and authorized.

- Resolve a terminal from live inventory or pane state.
- Respect frame, stream, queue, and connection limits from capabilities.
- Handle `terminal.frame`, `terminal.output_ready`, exit, close, and resync
  events by their exact terminal ID and sequence.
- Treat the control stream as an exclusive lease and release it promptly.
- Apply `terminal.frame` as a complete replacement at its captured revision;
  the acknowledgment revision is not an emitted-frame cursor. Reconnect for a
  fresh frame after EOF or resync, including when the child is quiet.
- Use typed literal, submit, and key actions instead of inventing escape
  sequences.
- Never replace semantic agent or pane commands with terminal control merely
  because the protocol exposes it.

The endpoint is a Unix socket on macOS and Linux and an owner-restricted named
pipe on Windows. Luvus does not expose a public TCP listener. Use the supported
SSH or proxy route for remote work rather than exposing the local endpoint.

## Failure and retry rules

Read the structured error code and preserve it in the report. A timeout does
not prove a mutation failed. After a lost or uncertain response, inspect live
state before retrying because input, prompts, starts, and closes can execute
twice. Retry read-only idempotent methods when appropriate; reconcile every
write against current revisions and identities first.

### Prompt wait observation

With `wait:true`, `agent prompt` (also `agent send`) requires a new `working` or
`blocked` transition before the requested `until` state can complete the wait.
An unchanged status, title flicker, or quiet output alone cannot complete it.
`observed_state` records the first active transition; `status` is the current state.
The absolute `timeout_s` covers both stages (default 300 seconds). Timeout returns
`matched:false`, `evidence:"timeout"`, and a null `observed_state` if no transition
was seen. Pane or terminal exit returns `agent_not_running` with `pane`, `queued`,
`submitted`, `observed_state`, `reason:"pane_closed"`, `baseline_revision`, and
`content_revision` under `error.data`. Timeout and pane exit during a wait use CLI
exit code 2. Cancellation, timeout, and exit release pending wait ownership.
Without `wait:true`, the immediate `submitted:true`, `evidence:"queued"` response is
unchanged and omits `observed_state`. Submission still means queue admission;
state transitions do not confirm consumption of the prompt text. Do not resend
automatically after a timeout or lost response because queued input may execute.
For `agent.send` and `agent.prompt`, detected blocked prompt evidence—including
in non-Codex panes—returns `agent_not_ready` before text or Enter is queued.
Startup, sign-in, selection, and approval screens are examples, not an
exhaustive list. A server-launched or restored Codex pane with an `agent_session`
also returns `agent_not_ready` when prompt evidence is Unknown, unless live Codex
composer geometry reports Ready. Existing Codex panes without that requirement
retain the permissive Unknown-evidence fallback. Inspect the visible screen and
use `agent.keys` only for an explicitly authorized interaction.

For UHP interactions that must match the inspected screen, use `agent.read`
with `source:"visible"` and pass its `content_revision` as `if_content_revision`
together with its `terminal_id` in `agent.keys` params. The revision is a
non-negative integer; the terminal ID is exactly 32 lowercase hex characters.
Both fields are optional as a pair; a one-sided or malformed pair is
`invalid_request`. A deferred pane has `terminal_id:null` and cannot be fenced.
An unavailable read snapshot has empty text and null coordinates.

The server checks the pair and queues keys under the same engine lock used to
capture the text. `content_revision_conflict` means no keys were queued: re-read
and reassess the authorized action, never retry the same pair. Generic response
`revision` / request `if_revision` are global event coordinates, not the pane's
content counter. Without the pair, behavior is unchanged. Older servers omit the
coordinates or reject the new fields; omit the pair only when legacy unfenced
admission is acceptable. These are UHP params, not CLI flags.

The fence covers queue admission only. Already queued input and child-side
changes not yet observed remain outside it. Cursor/SGR output can make a pair
stale even if the dialog text looks unchanged.

```json
{"id":"answer","method":"agent.keys","params":{"target":"reviewer","keys":["enter"],"if_content_revision":12,"terminal_id":"0123456789abcdef0123456789abcdef"}}
```

Replace the example coordinates with the actual `agent.read` result. The whole
key list validates before comparing the fence. A mismatched revision or terminal
identity, missing runtime, or unavailable engine sends nothing and reports
`content_revision_conflict` with expected/actual context; a closed writer still
returns `send_failed` for an otherwise matching pair.
