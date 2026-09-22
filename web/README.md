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

## Pair local and mobile devices

The bridge authorizes two browser devices by default. After the first browser
connects, open **Devices**, choose a limit from 1 through 8, and create a new
one-use pairing for each phone, tablet, or computer. Scan the locally generated
QR code with the phone camera, or use Copy/Share as a fallback. Existing devices
stay live; each new device receives an independent in-memory ticket that can
reconnect until its ticket expires or the bridge stops. QR generation happens
inside the browser and never sends the pairing secret to an external service.

For a bridge behind a private TLS tunnel, configure the public address so links
created from a localhost browser are immediately usable on a phone:

```sh
LUVUS_WEB_PUBLIC_URL=https://luvus.example.test \
LUVUS_WEB_ORIGINS=https://luvus.example.test \
LUVUS_WEB_MAX_DEVICES=3 \
npm --prefix web run dev
```

`LUVUS_WEB_MAX_DEVICES` sets the initial limit and accepts 1 through 8. The
Devices panel may change that limit for the current bridge lifetime, but cannot
set it below the number of authorized devices plus unspent pairing links.

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

Open the fragment-bearing URL printed by the bridge. Every browser pairing code
is one-use and is exchanged for an independent in-memory bridge ticket. Browser
tickets expire and are never persisted beyond that device tab's
`sessionStorage`.

In a controlled terminal, click the terminal or the keyboard button to focus
native input. Physical and mobile keyboards write directly to the PTY; shell
or agent history, cursor movement, and Tab completion therefore remain owned by
the child application. `Alt`/`Option`+Backspace and `Ctrl`+Backspace delete the
previous word; `Alt`/`Option`+forward Delete and `Ctrl`+Delete delete the next
word. `Command`+Backspace/Delete clear toward the start/end of the line, as do
`Ctrl+U`/`Ctrl+K`. The bottom dock only supplies keys that are awkward on
touch keyboards; Enter and Backspace remain on the native keyboard. On mobile,
the terminal follows the visual viewport and keeps the live cursor above the
software keyboard. Submitting input resumes follow-tail so streaming output
stays visible; an intentional touch or wheel scroll still pauses it for history
reading. Clipboard paste uses terminal bracketed-paste semantics and
does not add Enter. The `+` button, clipboard file paste, and drag/drop all
stream files up to 32 MiB in bounded chunks. Luvus stores the bytes privately
on the selected server and pastes only the resulting remote path, so the same
flow works through a remote bridge and never exposes a meaningless local
browser path to the PTY.

On Mission Control, select the **SESSION** name below Live stats to list known
Luvus sessions and move the web bridge to another namespace. Running sessions
are attached without disturbing their TUI clients or PTYs. In control mode, a
known stopped session may be started on selection; read-only bridges can switch
only to sessions that are already running. Because one bridge owns one selected
upstream, a switch moves every browser device connected to that bridge while
other TUI and CLI clients remain attached to their own sessions.

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
