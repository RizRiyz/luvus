# Luvus UHP 1.0

This package is the source-controlled Universal Harness Protocol 1.0 contract.

- `schema/request.schema.json` publishes every callable method.
- `schema/response.schema.json` defines success and error envelopes.
- `schema/event.schema.json` defines sequenced event frames.
- `schema/event-catalog.schema.json` maps general event names to their payload
  field schemas without rejecting future additive event names.
- `schema/terminal/` contains strict terminal method and stream components.
- `schema/access/descriptor.schema.json` defines transport-neutral remote
  bootstrap metadata; `access/README.md` defines provider and pairing behavior.
- `fixtures/` contains valid and invalid global wire examples.
- `terminal/fixtures/` exercises terminal identities, input, streams, and errors.

The installed binary embeds this package. Print it with `luvus uhp schema` and
query live methods and limits with `luvus uhp capabilities`.

Session-server methods use the owner-only local endpoint. The separate
`host.capabilities` profile is handled on demand by `luvus uhp proxy`, including
named-session lifecycle and confirmed host installation controls. It rejects
delegated session tokens and is not part of remote `uhp access`.

All requests use the `luvus-uhp` `1.0` identity. Method namespaces organize the
surface but do not define separate protocols or capability handshakes.
Optional delegated `auth` tokens contain 1 to 256 printable ASCII bytes.

The `automation.*` family controls durable agent schedules through the same
session-server contract. Calendar triggers retain an IANA timezone while wire
deadlines and occurrence keys use UTC Unix seconds. Read-only inspection uses
the `read` scope; definition changes and run requests require `orchestration`.
Create and run requests support bounded idempotency keys for safe retries.
