# Luvus Socket API 1.0

The Socket API is the complete server-owned Luvus control surface. It uses one
newline-delimited JSON request and response per connection. Event subscriptions
and Terminal observe/control requests are the documented exceptions: those
connections become bounded streams after their initial response.

UHP Runtime and Terminal remain independently versioned profiles on the same
endpoint. Their 1.0 contracts are not changed by this schema.

Run `luvus api socket-schema` for the exact installed contract, including the
strict Runtime and Terminal profile schema bundles, and
`luvus api socket-capabilities` against a running server for live discovery.

Socket API 1.0 also defines stable workspace/tab identities, optimistic
`if_revision` mutation guards, bounded `after_sequence` event replay, optional
scoped ephemeral tokens, and bounded `socket.stats` counters. These are
additive fields and methods; the protocol version remains 1.0.
