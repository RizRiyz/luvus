# UHP v1 Runtime profile

This package defines the Runtime profile of the
[Universal Harness Protocol](../../README.md). Its compatibility wire name is
`luvus-runtime` 1.0. UHP is the public protocol-family name and does not replace
that frozen identifier or the existing method namespaces.

The Runtime profile is the versioned, app-level automation contract for
observing one Luvus session and integrating agent lifecycle sources. It
complements the UHP Terminal profile: Terminal owns PTY lifecycle and exact
I/O, while Runtime owns workspace layout, agent evidence, authoritative state
leases, and semantic waits.

Inspect `runtime.capabilities`, then subscribe before requesting
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

JSON Schema string limits count Unicode code points, and the live server and
example consumer enforce the same unit. The newline-delimited transport retains
its separate UTF-8 byte limit for complete request and response frames.

## Methods

| Method | Responsibility |
| --- | --- |
| `runtime.capabilities` | Return the exact profile identity, methods, agent states, authority sources, event fence, and active limits |
| `session.snapshot` | Return the complete workspace, tab, pane, native-view, process, and agent-state snapshot with a sequence fence |
| `pane.processes` | Return bounded cached executable identities for one pane without exposing full arguments |
| `agent.explain` | Explain the identity and state evidence currently winning for one target |
| `agent.report` | Acquire or renew an authoritative agent-state lease for one pane and source |
| `agent.release` | Release the caller's authority lease explicitly |
| `agent.start` | Select or create a pane, queue one validated agent launch, reserve its name, and wait for detection as one server-owned workflow |
| `agent.prompt` | Atomically queue prompt text plus Enter and optionally wait for post-submission lifecycle or settled-output evidence |
| `agent.wait` | Wait for one pane to reach one semantic agent state within the announced bound |
| `events.subscribe` | Stream bounded sequenced session and agent events after acknowledgment |

Runtime capability discovery accepts no profile offer. A consumer calls
`runtime.capabilities`, then verifies that the response identifies
`luvus-runtime` 1.0 and honors only the announced methods and limits. The Luvus
application version is not a substitute for that check.

## Authority and event rules

An integration report is a renewable lease rather than a permanent label. One
source owns a pane at a time, sequence numbers increase, conflicting sources
are rejected, and a report expires when its TTL is not renewed. After release
or expiry, Luvus can fall back to process, launch, title, screen, and prior
identity evidence.

For a race-free initial state, subscribe first and request `session.snapshot`
on a second connection. Discard buffered events through the snapshot's
sequence fence, then apply later events in order. A gap, EOF, reconnect, or
server replacement requires a fresh subscription and snapshot.

The profile is bounded by the limits returned from `runtime.capabilities`.
Callers must cancel outstanding waits on disconnect and must never interpret
unavailable process evidence as proof that no child process exists.

`agent.prompt` returns `submitted:true` once the one-piece input action is
queued. A waiting call does not treat the target's pre-existing idle state as
completion. It first requires post-submission evidence: an observed Working
state, or a newer content revision that remains quiet for the bounded settle
window. This also covers fast turns that begin and finish between detection
ticks. A timeout reports `matched:false` while preserving `submitted:true`, so
consumers must inspect state or capture output and must never resend blindly.
Only one waiting `agent.prompt` may own a pane at a time. A conflicting request
is rejected before its input is queued, because terminal output has no native
turn identifier that could safely attribute one transition to two callers.

`agent.start` owns pane selection or creation, safe argument quoting, command
submission, name reservation, and readiness observation in one request. A
`ready:false` timeout does not kill the pane because the command may still be
starting; consumers can inspect it without repeating the launch.

## Installed contract and conformance

An installed binary prints the exact embedded profile with:

```sh
luvus api runtime-schema
luvus api runtime
luvus api session
```

The schema package and its fixture manifest are normative. Validate them
without importing Luvus by running:

```sh
python3 examples/runtime-api/consumer.py
```
