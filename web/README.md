# Luvus Web

Mobile-first access to a running Luvus session through a private bridge and the
public UHP contract. The browser never receives a Luvus owner socket, UHP
pairing code, or delegated UHP token.

## Quick development run

From the repository root:

```sh
npm --prefix web run dev
```

That one command installs missing web dependencies, builds the debug binary and
web packages, starts the isolated `web-dev` session with terminal control,
starts the bridge, and opens the one-use pairing URL in the default browser.
Press `Ctrl+C` to stop both the bridge and its isolated server.

Useful variants:

```sh
# Monitoring only; no terminal input
npm --prefix web run dev -- --read-only

# Full isolated integration test, including a server restart
npm --prefix web run test:live

# Clean up the default development session after an interrupted run
npm --prefix web run dev:stop
```

Override `LUVUS_WEB_SESSION`, `LUVUS_WEB_HOME`, or `LUVUS_WEB_PORT` when
another isolated profile or port is needed. Set `LUVUS_WEB_NO_OPEN=1` or pass
`--no-open` to print the URL without opening a browser. The ordinary
`LUVUS_SESSION` and `LUVUS_HOME` selectors remain supported outside a managed
Luvus pane.

## Manual development

Build Luvus first, then run an isolated server and the bridge:

```sh
cargo build
env -u LUVUS_SOCKET_PATH -u LUVUS_SESSION \
  LUVUS_HOME="$HOME/.luvus-dev" \
  ./target/debug/luvus --session web-dev server restart

cd web
npm install
npm run build
LUVUS_BIN="$PWD/../target/debug/luvus" \
LUVUS_HOME="$HOME/.luvus-dev" \
LUVUS_SESSION=web-dev \
npm start
```

Open the fragment-bearing URL printed by the bridge. The browser pairing code
is one-use and is exchanged for an in-memory bridge ticket. Browser tickets
expire and are never persisted beyond `sessionStorage`.

In a controlled terminal, click the terminal or the keyboard button to focus
native input. Physical and mobile keyboards write directly to the PTY; shell
or agent history, cursor movement, and Tab completion therefore remain owned by
the child application. The bottom dock only supplies keys that are awkward on
touch keyboards. Clipboard paste uses terminal bracketed-paste semantics and
does not add Enter. The `+` button, clipboard file paste, and drag/drop all
stream files up to 32 MiB in bounded chunks. Luvus stores the bytes privately
on the selected server and pastes only the resulting remote path, so the same
flow works through a remote bridge and never exposes a meaningless local
browser path to the PTY.

The bridge binds `127.0.0.1` by default and starts read-only access. Set
`LUVUS_WEB_CONTROL=1` to enable terminal control. To place the bridge behind a
TLS tunnel, keep the bridge loopback-bound and set `LUVUS_WEB_ORIGINS` to the
comma-separated public HTTPS origins accepted during WebSocket upgrade.

## Security boundaries

- Luvus owns state, PTYs, validation, and scoped UHP authority.
- The bridge owns browser authentication and injects UHP credentials upstream.
- The external provider owns TLS/WSS and reachability.
- The browser uses a separate, bounded protocol and cannot supply upstream
  authentication fields.

The bridge enforces origin checks, payload and connection limits, per-client
rate limits, bounded pending work, and outbound backpressure. It exits when its
child UHP access process exits, revoking upstream authority.
