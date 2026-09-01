# UHP Access 1

UHP Access is a transport-neutral bootstrap boundary for carrying Luvus UHP
1.0 through an independently chosen secure byte-stream provider.

Start it with:

```sh
luvus uhp access
luvus uhp access --control
luvus uhp access --ttl 7200
luvus uhp access --control --ttl 3600
luvus uhp access --no-expiry
luvus uhp access --control --no-expiry
```

`--ttl` configures the delegated authority lifetime in seconds from 1 through
86400. Without it, both read-only and control access last 24 hours. The one-use
pairing code lasts at most five minutes and is shortened automatically when the
authority lifetime is shorter. `--no-expiry` instead binds authority to the
foreground command: it remains valid until that process exits. Luvus rotates
bounded upstream tokens internally and revokes them when access stops; the
client token itself is accepted only by that loopback gateway.

The command writes exactly one JSON descriptor line to stdout and remains in
the foreground. The descriptor schema is
`../schema/access/descriptor.schema.json`. A provider forwards only the
descriptor's `127.0.0.1` TCP endpoint; it never receives the owner-only Luvus
socket or named pipe.

The first connection sends one pairing frame:

```json
{"type":"pair","code":"ABCD-EFGH-JKLM"}
```

The response returns an in-memory client token and its scopes. A finite session
includes `expires_at`; process-bound access includes `expires_on_close:true`.
Pairing succeeds once, expires after five minutes, and is rejected after five
failed attempts. Later connections carry ordinary LF-terminated UHP request
frames with that token in `auth`. Ordinary requests use one connection per
request. Event and terminal methods keep their connection open for the
advertised stream lifetime.

The provider must offer an authenticated, confidential, integrity-protected
byte stream and preserve order, half-close, EOF, and backpressure. It must not
rewrite JSON, terminate UHP authentication, persist credentials, expose the
loopback port directly, or fall back to a public plaintext listener. Stopping
the foreground command closes the endpoint and revokes its delegated token;
the Luvus server and panes remain alive.

UHP Access is not a new UHP version. Clients must discover live capabilities
after pairing and validate frames against the installed UHP schema bundle.
