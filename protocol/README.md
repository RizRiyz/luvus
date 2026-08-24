# Universal Harness Protocol

`uhp/v1` is the complete public automation contract for Luvus. It publishes
one protocol identity, request schema, response schema, event schema, fixture
set, method registry, and terminal component schemas.

UHP methods cover the full server. The `terminal.backend.*` prefix is a method
namespace inside UHP, not a second protocol. Unix sockets and Windows named
pipes are transports only. The private binary client protocol used to render
the TUI is not part of UHP.

Use the installed binary as the source of truth:

```sh
luvus uhp schema
luvus uhp capabilities
luvus uhp snapshot
```

The compatibility contract stays at UHP `1.0`. Additive method and schema work
must keep old valid requests valid. Breaking changes require a new UHP major.
