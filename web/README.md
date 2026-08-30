# Luvus Web reference client

This directory is an optional sample and test artifact, not a Luvus runtime
dependency or the canonical UHP client. It demonstrates Experimental browser
access to a native Luvus server. The read-only default
speaks UHP 1.0 through a private authenticated gateway and a Tailcat encrypted
tunnel. It is a standalone artifact: it is not built by or served from the
Astro marketing website.

## Build

Requirements:

- Bun 1.4.0
- Go 1.26.5 (the pinned Tailcat v0.2.0 requirement)

```sh
cd web
bun install --frozen-lockfile
bun run check
bun run build
```

The build verifies the Go module graph, compiles the Go/WASM adapter from
source, checks the 8 MiB compressed budget, hashes every executable asset, and
copies the canonical UHP 1.0 schemas into `dist/`. Generated files are ignored.

The browser UHP code depends only on `app/src/transport.ts`. Tailcat-specific
loading lives in `app/src/adapters/tailcat.ts`, and its Go/WASM implementation
lives in `wasm/`. Another client can provide the same ordered byte-stream
interface without importing either Tailcat component.

Serve `dist/` from an HTTPS origin that honors `dist/_headers`. The WebAssembly
file should be served as `application/wasm`; a deployment may serve its matching
precompressed `.gz` file with `Content-Encoding: gzip`.

## Host use

Install the exact reviewed Tailcat v0.2.0 binary, start or attach the selected
Luvus session, and run:

```sh
luvus web
```

Open the displayed URL and enter the address, port, and one-use pairing code.
The host command stays in the foreground and expires after 30 minutes. Ctrl-C
closes the gateway and Tailcat child and revokes the delegated UHP token; it
does not stop Luvus or its panes.

For an explicit 15-minute control session, run:

```sh
luvus web --control
```

Control is restricted to focusing existing workspaces, tabs, and panes,
prompting an existing detected agent, and opening a leased UHP control stream
for an existing terminal. The terminal sends bounded literal text, submitted
text, and reviewed logical keys. It cannot use one-shot raw pane methods,
launch or close anything, manage files or Git, fork or resume agents, or
manage tokens.

## Security boundary

- Plain `luvus web` has read-only UHP authority and may open a bounded terminal
  observation stream. `--control` adds the documented focus, prompt, and leased
  terminal-control methods; every other mutation and every token-management
  method is rejected by the gateway.
- The owner-only Luvus socket or named pipe is never exposed to Tailcat.
- Pairing expires after five minutes, succeeds once, and allows five attempts.
- Credentials remain in memory only; the page uses no storage, analytics,
  service worker, remote font, or third-party script.
- Tailcat is loaded only after Connect is selected. Ordinary Luvus startup and
  Cargo builds do not run Go, WASM, Tailcat, a gateway, or a network request.

Use a local DERP topology for integration testing. Do not use a production
Luvus server or public relay as a CI dependency.
