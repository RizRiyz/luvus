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
- `type_literal`, `paste_text`, `paste_image`, `submit_text`, `send_key`, and
  bounded `upload_*` control-stream actions
- `set_title` and `notify_terminal`
- `create_workspace`, `create_sibling`, and `close`
- `snapshot`, `events`, `wait_change`, and `wait_output`
- privacy-preserving cached `process_inspection`, returning executable names
  rather than full argument vectors that may contain secrets

Every control-stream action is advertised by its exact action name in
`terminal.capabilities`. Consumers must check those names individually and
must not infer image or file-upload support from `control_stream`, the protocol
version, or the Luvus release version. In particular, an older server without
`paste_image`, `upload_start`, `upload_chunk`, `upload_finish`, or
`upload_cancel` does not support that action.

The bounded `send_key` vocabulary includes `ctrl-w` and `alt-d` for backward
and forward word deletion plus `ctrl-u` and `ctrl-k` for deletion toward the
start and end of the current line.

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
`terminal.backend.control` adds newline-delimited input and upload actions on
that same connection. `paste_text` preserves terminal bracketed-paste semantics
without adding Enter. `paste_image` accepts one strictly validated base64 PNG
up to 160 KiB. General files use `upload_start`, ordered `upload_chunk` frames,
and `upload_finish` or `upload_cancel`; one control stream owns at most one
incomplete upload, each chunk is at most 160 KiB, and a file is at most 32 MiB.
Luvus stages bytes in private selected-session storage and pastes only the
completed server-owned path. Only one API control stream may lease a terminal
at a time. There are at most eight combined observe/control streams per server.
Overflow requires a fresh capture and reconnect.

Each `terminal.frame` replaces the previous capture. Its `content_revision`
belongs to the text captured under the terminal-engine lock. Both initial and
subsequent sends advance the stream cursor only to that emitted revision,
after a successful write; output arriving during a write remains eligible for
the next frame. The acknowledgment's revision is an earlier observation, not
proof that a frame has been emitted. Clients that see `stream_cursor` in
`terminal.features` may request `cursor:true`. Those frames add
`cursor.offset`, a Unicode-scalar offset in the normalized rendered text with
ANSI control bytes excluded, plus `cursor.padding_cells`, the number of blank
terminal cells trimmed immediately before the live cursor. A renderer inserts
those cells only before its visual caret; they are not captured output. `null`
means the live child cursor is hidden or outside the bounded capture. Without
that opt-in, the original strict UHP 1.0 frame shape is preserved.

An installed binary exposes the same contract with `luvus uhp schema`,
live negotiation with `luvus uhp capabilities`, the fenced inventory
with `luvus uhp snapshot`, and terminal events with
`luvus uhp events`.

The root UHP request schema remains authoritative. These components define the
additional identity, parameter, stream, and failure rules for terminal methods.
