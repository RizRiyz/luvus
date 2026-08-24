# Luvus UHP 1.0

This package is the source-controlled Universal Harness Protocol 1.0 contract.

- `schema/request.schema.json` publishes every callable method.
- `schema/response.schema.json` defines success and error envelopes.
- `schema/event.schema.json` defines sequenced event frames.
- `schema/terminal/` contains strict terminal method and stream components.
- `fixtures/` contains valid and invalid global wire examples.
- `terminal/fixtures/` exercises terminal identities, input, streams, and errors.

The installed binary embeds this package. Print it with `luvus uhp schema` and
query live methods and limits with `luvus uhp capabilities`.

All requests use the `luvus-uhp` `1.0` identity. Method namespaces organize the
surface but do not define separate protocols or capability handshakes.
