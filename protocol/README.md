# Universal Harness Protocol (UHP) v1

The Universal Harness Protocol is Luvus's public, harness-neutral automation
contract. It lets coding harnesses, orchestrators, editors, and local developer
tools observe a Luvus session, integrate agent state, and control real PTY
terminals without scraping the TUI or importing Luvus's Rust types.

UHP v1 is a protocol family with two profiles:

| Profile | Wire name | Owns | Normative package |
| --- | --- | --- | --- |
| Runtime | `luvus-runtime` 1.0 | session layout, agent evidence, authority leases, semantic waits, and runtime events | [`runtime/v1`](runtime/v1) |
| Terminal | `luvus-terminal-backend` 1.0 | PTY inventory, stable terminal identity, bounded capture, exact input, lifecycle, and terminal events | [`terminal-backend/v1`](terminal-backend/v1) |

The profile names and RPC namespaces are compatibility identifiers. UHP is the
public name for the complete contract, not a new `uhp.*` method namespace. A v1
consumer must negotiate or inspect each profile independently and must not
replace either wire name with `uhp`.

## Which profile to use

Use the Runtime profile when a harness needs to understand Luvus as a session:

- enumerate workspaces, tabs, panes, native views, and agent state
- obtain cached process executable identities without exposing full arguments
- explain how Luvus identified an agent or its state
- report authoritative agent state through a bounded renewable lease
- wait for an agent to reach `idle`, `working`, `blocked`, or `done`
- subscribe to sequenced session and agent events

Use the Terminal profile when a harness needs to control a PTY:

- inventory every live terminal across all workspaces
- validate a terminal and its root process lifetime before mutation
- capture bounded visible or recent text
- observe safe ANSI frames and optionally control input on one live connection
- type literal text, submit text with Enter, or send one logical key
- create, label, notify, wait on, or close a terminal
- subscribe to bounded terminal lifecycle and output events

Most full harness integrations use both. A consumer that only reports agent
state can implement the Runtime profile alone. A terminal driver can implement
the Terminal profile without knowing Luvus's agent manifests.

The broader Luvus socket API includes user-facing features such as tabs, Git,
DIFF, files, modules, and UI surfaces. Those methods remain public API, but they
are not part of the stable UHP v1 contract unless they appear in a normative
UHP schema.

## Discovery and transport

UHP v1 uses the existing owner-only local Luvus control endpoint. It does not
open a network listener.

1. Run `luvus session list --json` as a discovery-only operation.
2. Keep only a running session selected by the user or caller.
3. Validate the discovery-supplied endpoint before connecting.
4. Negotiate or inspect the required UHP profile.
5. Retain the returned server generation and sequence fences.

macOS and Linux publish a Unix socket. Windows publishes a local named-pipe
address. Discovery is a hint, not proof of ownership. Terminal consumers must
apply the platform checks in
[`terminal-backend/v1/endpoint-validation.md`](terminal-backend/v1/endpoint-validation.md)
before trusting endpoint metadata.

Ordinary requests use one UTF-8 JSON object and one JSON response per
connection, each terminated by LF and bounded to 1 MiB. A successfully
negotiated event subscription or Terminal observe/control request changes only
that connection into a bounded stream. It does not turn the endpoint into a
general multi-request session.

Consumers that cannot open the native transport can forward one bounded frame
through `luvus socket proxy`, including over SSH. Native sockets or named pipes are
required for all streams and are preferred for high-frequency local calls.

## Safe startup sequence

A race-free full integration uses this order:

1. Discover and validate one session endpoint.
2. Call `runtime.capabilities` and check the returned `luvus-runtime` version,
   methods, states, authorities, and limits.
3. Call `terminal.backend.capabilities` with the exact
   `luvus-terminal-backend` 1.0 offer and retain `server_generation`.
4. Subscribe to the event profile needed by the integration.
5. Fetch the matching snapshot on a second connection.
6. Discard buffered events through the snapshot's sequence fence.
7. Apply later events in order. Resnapshot after a gap, overflow signal, EOF,
   reconnect, or server-generation change.

Never infer identity from a title, pane position, prompt, or rendered TUI row.
Runtime pane IDs are routes. The Terminal profile additionally supplies a
random `terminal_id` for one successful PTY lifetime and a random
`server_generation` for one server lifetime. Identity-sensitive terminal
mutations require the complete current tuple.

## Failure and retry rules

Read operations can be repeated after reconnecting and revalidating the
endpoint. Mutations require stricter handling:

- a structured rejection with `dispatch: not_started` is safe to correct and
  retry
- `dispatch: rejected` means the current target or state was refused
- a lost response after a mutation is `possibly_executed`
- never replay a possibly executed mutation automatically
- reconcile through a fresh snapshot or inventory before deciding what to do

This rule prevents duplicate text submission, repeated process creation, and
closing the wrong terminal after a route or server changes.

## Versioning

The Luvus application version and UHP profile versions are independent. A
consumer must use capability responses rather than an app-version check.

- additive optional capabilities may be introduced without breaking v1
- a consumer calls only methods and limits announced by the connected server
- an incompatible request or semantic change requires a new profile major
- wire names, identity meanings, framing, and failure semantics stay stable
  for profile version 1.0

There is no aggregate UHP handshake in v1. Supporting one profile does not
imply support for the other.

## Normative contract and conformance

The schemas, fixtures, manifests, endpoint rules, and versioned READMEs below
this directory are normative. Application source, website prose, and example
adapters cannot silently redefine them.

The repository provides dependency-free fixture consumers, mock servers, live
lifecycle checks, failure injection, Windows ConPTY coverage, and an opt-in
1/10/50-pane performance benchmark. Start with the
[Terminal conformance guide](terminal-backend/v1/conformance/README.md) and the
fixture manifest in each profile package.

An integration is ready only when it:

- validates every published valid and invalid fixture it consumes
- negotiates exact profile identities and honors announced limits
- validates local endpoint ownership on its target platforms
- implements subscribe-first snapshot reconciliation
- treats EOF and sequence gaps as possible event loss
- preserves terminal identity across moves and rejects stale routes
- never logs prompts, captures, command arguments, cwd, or agent messages by
  default
- has failure tests for reconnects, lost mutation responses, stale identities,
  timeouts, and server replacement
