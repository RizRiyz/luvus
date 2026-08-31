# AGENTS.md

Shared maintainer guidance for coding agents working in the Luvus repository.
This file is the repository-level source of truth. Tool-specific instruction
files may add guidance, but they must not contradict it.

## Start with current evidence

Luvus changes quickly. Before describing behavior or editing a subsystem:

1. Check `git status --short --branch` and preserve every unrelated change.
2. Inspect the current implementation and its focused tests.
3. Use `cargo run --quiet -- help all` or `src/cli.rs` for the installed command
   surface.
4. Use `website/src/content/docs/` for public product documentation.
5. Treat ignored files under `docs/` as plans and historical handoffs, not as
   proof that a feature exists.

When remote state matters, compare the checkout with `origin/main`. Do not
silently change branches, fetch, rebase, or rewrite the user's work merely to
make the checkout match upstream.

## Product model

Luvus is mission control for AI coding agents. It is a single Rust binary with
several roles:

- With no command, it starts or attaches a thin TUI client.
- A detached server owns workspaces, tabs, panes, PTYs, terminal grids, agent
  state, persistence, modules, orchestration, API dispatch, and rendering.
- CLI commands send bounded JSON requests to the selected server and exit.
- Named sessions are independent server namespaces selected with
  `--session <name>` or `session attach <name>`.
- `--local` is a monolithic development escape hatch.
- `--remote <host>` attaches through an SSH byte bridge.

There are two local endpoints per server:

- the newline-delimited JSON control API;
- the binary client transport for input, full frames, frame diffs, clipboard,
  notifications, sound, detach, and shutdown messages.

Clients are disposable. The server is the single writer of application state.
Detaching a client leaves panes alive. Stopping the server ends its live PTYs;
the next server restores layout and resumes supported native agent sessions
where possible.

## Non-negotiable development safety

- Work in the current project directory. Do not create another worktree, clone,
  or development directory under `/tmp`, `/private/tmp`, or elsewhere unless
  the user explicitly requests it.
- Never stop, restart, attach to, benchmark, or delete the user's production
  Luvus server during development.
- Debug builds normally use `~/.luvus-dev/`; installed release builds use
  `~/.luvus/`. An inherited `LUVUS_SOCKET_PATH` or session selector can still
  route a debug command to another running server.
- Before a debug lifecycle or integration test, run outside a managed Luvus
  pane or explicitly isolate the home and remove inherited socket/session
  selectors. On Unix, a safe pattern is:

  ```bash
  env -u LUVUS_SOCKET_PATH -u LUVUS_SESSION \
      LUVUS_HOME="$HOME/.luvus-dev" \
      ./target/debug/luvus --session agent-test server restart
  ```

- Use the exact binary under test. Confirm the executable, selected home,
  session, and socket before interpreting runtime results.
- Never use destructive Git commands, discard user changes, or stage unrelated
  files.
- Do not commit, amend, push, tag, open a PR, merge, publish, or edit releases
  unless the user explicitly authorizes that action.
- Do not add AI co-author trailers unless the user explicitly requests one for
  that commit.
- After implementation, report changed files, verification performed, remaining
  caveats, and a copyable Conventional Commit message.

## Architecture and code map

`src/main.rs` performs process-role and session routing. The important ownership
boundaries are:

- `src/app/`: `App`, input, dispatch, settings, dashboards, workers, and domain
  state. Application mutations converge on the server/local event-loop thread.
- `src/layout.rs`: pure binary space-partition pane geometry. Layout leaves store
  IDs; live panes and native views remain flat in `App`.
- `src/terminal/pty.rs`: portable PTY creation, reader/writer/reaper lifecycle,
  environment propagation, and cancellation.
- `src/terminal/vt/`: the `VtEngine` boundary and current Alacritty adapter.
  `vendor/vte/` and `vendor/alacritty_terminal/` are intentional local patched
  crates published under Luvus package names.
- `src/ipc/server.rs`: headless event loop, client ownership, rendering cadence,
  backpressure, and frame delivery.
- `src/ipc/client.rs`: terminal client, input forwarding, frame application, and
  client-side effects.
- `src/ipc/api.rs` and `src/ipc/transport.rs`: bounded local API handling plus
  Unix-socket and Windows-named-pipe transport.
- `src/ui/`: Ratatui rendering, panes, sidebars, docks, tab bar, settings,
  Git/files/diff, Mission Control, overlays, and hit-test geometry.
- `src/detect.rs`: agent identity and state evidence. Detection is native Luvus
  behavior and must not depend on an installed agent skill.
- `src/agent.rs`: stable native-session, resume, fork, and usage facade.
  `src/agent/registry.rs` is the immutable built-in descriptor registry;
  `src/agent/types.rs` defines its operation records. Every compiled-in agent
  owns `src/agent/<agent>/`, including detection-only agents. Put non-trivial
  discovery, integration, and embedded assets in that directory, not in the
  registry or facade.
- `src/integration.rs`: shared safe-editing mechanics and the optional
  agent-integration facade. Agent-specific hooks, plugins, extensions, paths,
  and lifecycle semantics belong to their adapter. Hooks augment detection;
  they do not replace it.
- `src/cli.rs`, `src/api/`, and `src/app/dispatch.rs`: parsing/help, public UHP
  contracts, and validated state mutation for one control surface.
- `src/config.rs` and `src/persist.rs`: configuration, migration, selected-session
  paths, snapshots, and restore.
- `src/git/`, `src/diff/`, and `src/files/`: GitHub/local Git data, semantic diff
  review and notes, and file browsing.
- `src/module/`: manifest-driven extensions that run out of process and call the
  same local API as other clients.
- `src/bar/`: Top and Bottom Luvus Bar declarations and rendering.
- `src/mission/`: usage and pricing data for Mission Control.
- `src/orch/` and `src/app/board.rs`: tasks, leases, quality gates, worktrees,
  and multi-agent orchestration.
- `src/logging/`: bounded, private, redacted, rotating runtime logs.
- `src/platform.rs` and `src/platform/windows.rs`: operating-system boundaries.

The main concurrency invariant is one mutable `App` owner. Background threads
may perform PTY, filesystem, Git, process, session, module, or network work, but
must return results through `AppEvent` or another bounded channel. The terminal
engine's narrow `Arc<Mutex<dyn VtEngine>>` sharing is an exception; keep its
lock scopes short and never hold it across unrelated slow work.

## State and public-contract invariants

- A normal layout leaf belongs to a live pane or a native view, never both.
- Workspace indices are 0-based at public boundaries. Tab positions are
  1-based publicly and 0-based internally. Pane IDs are stable opaque IDs for a
  server lifetime. Keep those conventions explicit in validation and docs.
- A server can remain alive with no workspace. Code that assumes an active
  workspace must guard that state before calling helpers such as `ws()` or
  `layout()`.
- Every client has its own viewport and render baseline. Passive or differently
  sized clients must not overwrite shared interactive geometry, focus, PTY size,
  cursor state, or scroll position.
- Validate all API input before mutation. Keep errors structured and keep
  ordinary requests to one newline-delimited JSON request and response.
- Owner-only socket/named-pipe security, peer validation, frame limits, bounded
  waits, and process identity checks are security boundaries, not optional
  cleanup.
- User configuration and snapshots must remain forward-tolerant through serde
  defaults and conservative migrations. Never hardcode a maintainer's home
  path, username, agent installation, or terminal.

## CLI API UHP and documentation parity

`luvus help all` is the command inventory. Major surfaces include workspaces,
tabs, panes, agents, files, Git, semantic diff notes, Mission Control,
worktrees, tasks, leases, modules, bars, UI docks, themes, sessions, skills,
integrations, search, waits, logs, and UHP.

When adding or changing a user-visible control:

1. Update CLI parsing, compact help, focused subcommand help, and validation.
2. Update API dispatch and response/error behavior.
3. Update UHP capabilities and schema when the control is public automation.
4. Add parser and dispatch tests, including indexing and pass-through arguments.
5. Update the relevant files under `website/src/content/docs/docs/reference/`
   and any affected guide.
6. Update the bundled agent guidance when an automation workflow changed:
   `skills/luvus/`, `plugins/luvus/skills/luvus/`, and
   `website/public/agent-readme.md`.

Human-facing CLI text is localized in `src/i18n/cli.rs`. Settings text is in
`src/i18n/settings.rs`. Command names, flags, JSON fields, UHP methods, paths,
and literal user data stay canonical. Do not ship a partial translation for a
registered language.

## Agent skills and integrations

Do not use the bundled Luvus skill as a substitute for reading this codebase.
The skill is a user-facing automation interface for controlling a running Luvus
instance and delegating work. Use it only when the user explicitly asks to
operate Luvus or delegate through it.

The supported user model is:

```text
luvus skill enable
luvus skill status
luvus skill disable
luvus skill show
```

`src/skill.rs` is authoritative for installation and compatibility behavior.
Do not manually copy skills into agent directories as an implementation fix.
Agent detection and sidebar status must continue to work without skills or
hooks installed.

## Adding or changing built-in agent support

First choose the smallest correct support level:

- A user or managed detection manifest can add identity and screen-state rules
  without recompiling Luvus. That agent is detection-only and must not silently
  gain session parsing, command execution, integrations, or skill installation.
- Native discovery, resume, fork, usage, or an integration requires built-in
  Rust code that has been reviewed and an owning `src/agent/<agent>/` adapter.
- A module or external UHP reporter is preferable when the feature does not
  need trusted in-process access to an agent's private native store.

For a built-in adapter:

1. Create `src/agent/<agent>/mod.rs` and assemble one immutable
   `AgentDescriptor`. Keep the canonical `id` lowercase and stable; aliases
   normalize user input but do not create additional agents. Set
   `launch_command` to the exact executable for a fresh interactive session;
   do not put flags or shell syntax in this field. Put a required static
   ORCH prompt subcommand or flag such as `ask` in `task_prompt_args`; never
   embed user data.
2. Declare identity evidence accurately. Put unmistakable executable names in
   `distinct`, ordinary words in `ambiguous`, versioned executable logic in a
   narrow `binary_matcher`, and exact interpreter package names—including npm
   scope—in `interpreter_packages`. Use `overlap_priority` only for a reviewed
   collision such as OMP versus Pi; never rely accidentally on registry order.
3. Add `sessions.rs` only when the upstream agent has a stable native store.
   Keep reads bounded and offline, match canonical CWDs portably, obtain session
   IDs from structured metadata, and declare only operations the upstream CLI
   safely exposes. `sessions.fork = None` is the correct value when no external
   native fork exists.
4. Add `integration.rs` and assets only for an optional, documented upstream
   hook/plugin/extension surface. Install and uninstall must be idempotent and
   surgical, preserve unrelated configuration and secrets, and leave native
   detection functional when the integration is absent.
5. Register the descriptor once in `src/agent/registry.rs`. Add it to the
   presentation-ordered integration projection only when `integration` exists.
   Do not add agent-name matches to UI, IPC, dispatch, Settings, or CLI code.
6. Keep generic process/title/screen authority and manifest merging in
   `src/detect.rs`. Built-in screen-state rules currently live there. Native
   Mission Control usage readers currently live behind `src/agent/usage.rs`;
   add one only when stable persisted counters exist, and never estimate token
   usage from transcript prose.

Required parity work:

- Add registry tests for unique IDs, aliases, interpreter packages, capability
  projection, presentation order, and every intentional identity overlap.
- Add detection fixtures for direct binaries, interpreter launchers, scoped
  packages, wrappers, false-positive prose, and both Unix and Windows path
  forms. Detection must work with skills and integrations absent.
- Add bounded session-store fixtures and resume/fork command tests when those
  operations exist. Add temporary-home install/status/uninstall tests for an
  integration, proving unrelated user files survive.
- Exercise managed/user manifest precedence, reload, and a manifest-defined
  unknown agent whenever identity plumbing changes.
- Update `README.md`, the supported-agent reference and guide, Settings/CLI
  integration projections, and the homepage grid when user-visible support
  changes. If automation behavior changes, also update UHP/schema fixtures,
  both bundled skill copies, and `website/public/agent-readme.md`.
- Run focused adapter/detection/integration tests, formatting, strict Clippy,
  the locked suite, and the locked release build. Use isolated live tests only
  for third-party agents actually available, and do not claim an untested OS.

Descriptors are static metadata. Adding an adapter must not add a dependency,
thread, timer, watcher, per-pane scan, network request, or render-path work.
The contributor-facing walkthrough is
`website/src/content/docs/docs/extend/adding-agent-support.mdx`; ignored
`docs/107-modular-agent-adapter-architecture.md` records the migration design,
not the current public contract.

## Modules and dependencies

Prefer a module when a feature can live outside core without weakening the user
experience. Modules are directories with `luvus-module.toml`, executable argv
commands, settings, actions, event hooks, panes, docks, and bar widgets. They
receive canonical `LUVUS_*` context. They must not receive direct in-process
access to `App`.

New dependencies require a concrete benefit and review of maintenance,
licensing, supply-chain exposure, binary size, compile time, and cross-platform
support. Prefer existing core dependencies and owner-maintained upstream crates.
Do not replace the patched terminal crates casually.

## Performance expectations

Luvus should remain fast and memory-efficient with many panes and agents:

- Keep the app loop event-driven. Avoid unconditional polling and periodic full
  fleet scans.
- Never run Git, GitHub, filesystem traversal, process discovery, module
  execution, or network work synchronously on the app loop.
- Avoid per-frame allocation, cloning whole terminal grids, and rendering when
  no visible client state changed.
- Hidden PTY output may update terminal state without forcing a whole-client
  render. Preserve frame coalescing and one-pending-frame backpressure.
- Keep terminal history bounded by the Scrollback Memory setting. The current
  Alacritty adapter maps a byte budget to a conservative row count, so reported
  memory values are estimates rather than exact allocator usage.
- Keep caches generation-based or event-invalidated, with explicit bounds.
- Compare performance using equivalent layouts, terminal sizes, workloads, and
  warm-up. Separate idle CPU, active CPU, physical footprint, live heap, peak,
  thread count, descriptors, and retained history. Debug and release results
  are not interchangeable.

## Cross-platform behavior

- Keep platform code behind narrow `cfg` boundaries and fail closed when the OS
  cannot prove process, path, or peer identity.
- Windows child processes launched by detached Luvus must not flash console or
  PowerShell windows. Reuse the no-window process helpers.
- Preserve Windows modifier, AltGr, IME, path, named-pipe, and npm-shim behavior.
- Preserve Unix socket ownership and permissions, signal/process lifecycle, and
  long-socket-path handling.
- A platform-specific fix needs tests on that platform when available and must
  not silently change macOS/Linux/Windows behavior outside its scope.

## Testing and verification

Use the narrowest relevant test while iterating. Do not repeatedly run the full
suite for a small local change.

```bash
cargo build
cargo test <focused-name> -- --nocapture
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
```

Run formatting after Rust edits. Before final handoff, run focused regression
tests plus the broad checks proportionate to risk. Platform, PTY, IPC, rendering,
and lifecycle changes also need a real debug-client/server test in an isolated
development home. Do not claim an untested platform is verified.

The CI matrix currently covers formatting, Clippy, locked tests on Ubuntu and
macOS, targeted Windows protocol/ConPTY boundaries, UHP fixtures and live
conformance, patched terminal crates, packageability, RustSec audit, and Nix
flake evaluation/build.

## Repository and contribution conventions

- `website/` is the Astro site and public documentation source.
- `.github/` contains CI, release automation, issue templates, and the PR body
  template.
- `docs/` and `CLAUDE.md` are intentionally ignored local maintainer material.
- `changelog/<version>.md` is embedded by the binary and feeds release content.
- `community/themes/` contains reviewed community themes.
- `protocol/uhp/v1/` is the versioned public automation contract.

Keep changes focused on one user-facing outcome. Avoid opportunistic cleanup.
Use concise Conventional Commit messages such as:

```text
fix(input): preserve navigation modifiers on Windows
feat(cli): add pane move command
perf(render): skip unchanged client projections
docs: clarify module setup
```

Before calling work complete, inspect the final diff, confirm no unrelated file
entered it, and explain what was verified and what remains unverified.
