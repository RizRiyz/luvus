# Luvus runtime API v1

This package defines the versioned, app-level automation contract for observing
one Luvus session and integrating agent lifecycle sources. It complements the
terminal-backend protocol: terminal backend owns PTY lifecycle and exact I/O;
runtime API owns workspace layout, agent evidence, authoritative state leases,
and semantic waits.

Negotiate with `runtime.capabilities`, then subscribe before requesting
`session.snapshot`. The snapshot carries the event-sequence fence needed to
discard older buffered events. Agent reports are ephemeral leases, one source
owns a pane at a time, and sequence numbers must increase. Full process argument
vectors are never returned.

The JSON Schemas are normative. Valid and invalid fixtures are checked by a
dependency-free consumer independently of the live app, and the same schema
bundle is embedded in every Luvus binary. Runtime event envelopes are sequenced
and bounded; consumers subscribe first, request `session.snapshot` on another
connection, discard events through its fence, then apply later events or
resnapshot after loss.
