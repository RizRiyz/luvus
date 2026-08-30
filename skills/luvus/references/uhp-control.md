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
features, and server identity rules. The schema defines exact request and
response fields. Do not infer a method from a newer website or binary.

`luvus uhp proxy` accepts one newline-delimited request from stdin and emits one
response. A request has `id`, `method`, and `params`; add `auth` only for an
explicit delegated token. Keep the inherited or explicitly selected session
and socket when invoking it.

`luvus uhp access [--control]` instead creates a temporary authenticated
loopback gateway for a persistent third-party byte-stream provider. Its first
stdout line is the access descriptor; it never contains the delegated token.
The client pairs once, then uses the returned token on ordinary UHP frames.
Keep the loopback endpoint private and require an encrypted, authenticated
transport.

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
- Mission Control: `mission.open`
- Worktrees and orchestration: `worktree.*`, `task.*`, and `lease.*`
- Extensions: `module.*`
- Themes and configuration: `theme.*`, `config.*`, and `manifest.reload`
- UI surfaces: `ui.sidebar`, `ui.dock.*`, `ui.bar.*`,
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
