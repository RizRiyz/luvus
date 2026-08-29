# Contributing to Luvus

Thanks for helping make Luvus better. Bug fixes, documentation, tests, and new
features are all welcome.

## Before you start

Small fixes can go directly to a pull request. For a large UI, architecture, or
behavior change, open an issue first so we can agree on the direction before you
spend time building it.

Functionality around Luvus, such as panels, integrations, and automations, may
fit better as a module than a core change. See [MODULE-GUIDE.md](MODULE-GUIDE.md).
Modules live in separate repositories, can use any language, and need no SDK.

## Development setup

Luvus requires Rust 1.88 or newer. Fork the repository on GitHub, then clone
your fork and create a focused branch:

```bash
git clone https://github.com/RizRiyz/luvus.git
cd luvus
git switch -c fix/short-description
cargo build
```

Debug builds keep their state in `~/.luvus-dev/`, separate from your installed
Luvus session in `~/.luvus/`.

For the quickest local UI run:

```bash
cargo run -- --local
```

For changes involving the client, server, sockets, or keyboard input, test the
real debug client and server from a normal terminal outside your production
Luvus session:

```bash
cargo run -- server restart
cargo run
```

Running a debug Luvus inside an older production Luvus pane can hide input bugs
because the outer process handles the keys first.

## Development guidelines

- **Keep changes focused.** Solve one user-facing problem at a time. Keep
  unrelated formatting, cleanup, and refactors separate.
- **Performance first.** Avoid per-frame allocations, unbounded scans, and
  blocking work on the event loop. Shell commands and filesystem scans belong
  on worker threads.
- **Keep state predictable.** Application state is separate from runtime and IO.
  Preserve the single event loop and follow the patterns around the code you edit.
- **Preserve user sessions.** Development and tests must not modify production
  state, close active panes, or connect to the production socket unexpectedly.
- **Keep behavior cross-platform.** CI builds Luvus on Linux, macOS, and Windows.
  Keep operating-system code in `platform.rs` behind `cfg` gates when possible.
- **Update related documentation.** CLI, API, configuration, or visible behavior
  changes should update the matching public documentation.
- **Keep CLI translations complete.** Command names, flags, JSON, and UHP stay
  canonical. Add human CLI text to `src/i18n/cli.rs` with all eight language
  values, preserve placeholders and literal user data, and run the focused CLI
  localization tests. Do not add a partial English fallback for a registered
  language.

## Tests and checks

Add a regression test for behavior changes when practical. Use
`cargo test <substring>` while developing to run one focused test.

Before submitting your change, run the same checks as CI:

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
```

UI tests render into an off-screen buffer, and many terminal tests exercise real
PTYs without requiring an interactive terminal. Test visible changes manually,
measure performance changes before and after, and test platform-specific code on
the affected platform when available.

## Adding agent support

Use a detection manifest when an agent only needs identity and live-state
recognition. Built-in session discovery, resume, fork, usage, or lifecycle
integration belongs in one modular adapter under `src/agent/<agent>/`; do not
spread new agent-name branches across the UI, CLI, IPC, or Settings.

The [Adding Agent Support](website/src/content/docs/docs/extend/adding-agent-support.mdx)
guide covers the descriptor fields, scoped interpreter packages, manifests,
session and integration boundaries, registry entry, documentation parity, and
required cross-platform tests. Detection must work without installing a skill
or hook, and optional integrations must preserve unrelated user configuration.

## Commits

Use concise Conventional Commit messages:

```text
fix(input): preserve navigation modifiers on Windows
feat(cli): add pane move command
perf(render): avoid repeated frame allocations
docs: clarify module setup
```

Luvus squash-merges pull requests, so follow-up commits during review are fine.
You do not need to rebuild a perfect commit history before submitting changes.

## Review

Maintainers may ask for a regression test, a smaller scope, or another
compatibility check. Keep review discussions focused on the behavior being
changed and push follow-up commits to the same branch.

## Reporting bugs

Include clear reproduction steps, your OS and terminal, and the output of:

```bash
luvus server status
luvus doctor
```

Screenshots and the name of the running agent are often helpful. For security
issues, follow [SECURITY.md](SECURITY.md) and do not open a public issue.
