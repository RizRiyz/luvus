# UHP 1.0 terminal namespace

This directory contains the strict terminal method and stream components of
[Universal Harness Protocol](../../README.md). It uses the global `luvus-uhp`
1.0 identity and the `terminal.backend.*` method namespace.

The namespace controls live Luvus terminals over the existing local control
endpoint. Protocol 1.0 uses one
UTF-8 JSON request and one JSON response per ordinary connection. The event
and terminal observe/control methods switch their connections into bounded
streams after one acknowledgment.
Each frame is terminated by LF and is at most 1 MiB including that LF.

Start by running `luvus session list --json`, keep only running sessions, and
use the returned `endpoint` descriptor. macOS and Linux announce
`unix_socket`; Windows announces `windows_named_pipe`. Discovery data is only a
hint until the endpoint passes the platform checks in `endpoint-validation.md`
and returns a compatible `uhp.capabilities` response.

Every request requires `id`, `method`, and `params`; `auth` is optional. Request
IDs contain 1 to 128 ASCII letters, digits, `.`, `_`, `:`, or `-`. Unknown or
duplicate fields are rejected. An `auth` token contains 1 to 256 printable
ASCII bytes. Every successful response has `id` and `result`; every failure has
`id` and `error`.

The authoritative limits and method shapes are in `schema/`. Examples in
`fixtures/` are indexed by `fixtures/manifest.json`. The standard-library
consumer in `examples/uhp/terminal/consumer.py` validates those fixtures
without importing Luvus or running its binary.

Terminal identity is the tuple `server_generation`, `terminal_id`, and the
current `pane_id` route. The first two values are independent random 128-bit
lowercase hexadecimal strings. Never retry a mutation after losing its response:
the client must classify that transport outcome as possibly executed and
reconcile through a fresh inventory.

Protocol v1 capabilities are:

- `inventory`, `validate`, `capture`, `observe`, and `control_stream`
- `type_literal`, `submit_text`, and `send_key`
- `set_title` and `notify_terminal`
- `create_workspace`, `create_sibling`, and `close`
- `snapshot`, `events`, `wait_change`, and `wait_output`
- privacy-preserving cached `process_inspection`, returning executable names
  rather than full argument vectors that may contain secrets

Protocol 1.0 capture includes a monotonic `content_revision` and provides a
sequence-fenced snapshot, bounded terminal-only event streams, and event-driven
waits. For a race-free initial view, subscribe first, fetch a snapshot on a
second connection, discard buffered events through the snapshot's
`event_sequence`, then apply later events. If a slow event connection closes,
resubscribe and repeat that snapshot reconciliation rather than assuming no
events were lost.
Each stream holds at most 256 queued events and one server accepts at most 64
simultaneous event subscribers. On queue overflow the server attempts one
`terminal.resync_required` control event and closes the stream. EOF is also
treated as possible loss because a blocked transport may prevent that final
control frame from reaching the client. `output_ready`, `metadata_changed`, and
`closed` can be replayed onto an existing snapshot. `created`, `moved`, and
`exited` intentionally stay lightweight and require a fresh snapshot when they
arrive after its fence. The dependency-free reference consumer implements and
tests this reconciliation policy.

`terminal.backend.observe` sends one safe normalized ANSI `terminal.frame`
immediately, then only after that terminal's existing coalesced
`terminal.output_ready` event advances its content revision. A stream has a
two-frame queue, captures at most 200 rows and 64 KiB, and never polls a PTY.
`terminal.backend.control` adds newline-delimited `type_literal`, `submit_text`,
and `send_key` action frames on that same connection. Only one API control
stream may lease a terminal at a time. There are at most eight combined
observe/control streams per server. Overflow requires a fresh capture and
reconnect.

An installed binary exposes the same contract with `luvus uhp schema`,
live negotiation with `luvus uhp capabilities`, the fenced inventory
with `luvus uhp snapshot`, and terminal events with
`luvus uhp events`.

The root UHP request schema remains authoritative. These components define the
additional identity, parameter, stream, and failure rules for terminal methods.
