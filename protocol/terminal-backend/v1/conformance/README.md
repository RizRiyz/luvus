# Conformance

Run the dependency-free fixture check from the repository root:

```sh
python3 examples/terminal-backend/consumer.py --fixtures
```

To query an explicitly selected development endpoint without performing a
mutation:

```sh
python3 examples/terminal-backend/consumer.py --socket ~/.luvus-dev/luvus.sock
```

Or let the example run the exact discovery-only command and inspect every
running Unix session:

```sh
python3 examples/terminal-backend/consumer.py --discover
```

The example sends only capability and inventory reads. Never point mutation
tests at an installed production session. A live CI conformance run must use a
dedicated `LUVUS_HOME` and terminals created for that run.

Run the fixture-backed mock and example consumer together:

```sh
python3 examples/terminal-backend/mock_conformance.py
```

After building the debug binary, run the live lifecycle suite. It creates its
temporary `LUVUS_HOME` only below this checkout's `target/` directory:

```sh
cargo build --locked
python3 examples/terminal-backend/live_conformance.py
```

The live suite negotiates 1.0, rejects an incompatible version, inspects cached
process identity, subscribes before create, checks lifecycle events, uses
`wait_output` instead of polling capture, verifies the fenced snapshot, and
closes only the terminal it created.

On Windows, build the debug binary and run the independent PowerShell consumer:

```powershell
cargo build --locked
.\examples\terminal-backend\live_conformance.ps1 `
  -Luvus .\target\debug\luvus.exe
```

The Windows suite validates discovery of the actual local named-pipe address,
exact 1.0 negotiation, an incompatible-version rejection, ConPTY creation,
the process creation marker, process discovery, input/output, lifecycle events,
and close. CI runs fixture, mock, and live conformance on Linux, macOS, and
Windows. Every live suite keeps its isolated state below this checkout's
`target/` directory.
