# terminal conformance

Run the dependency-free fixture check from the repository root:

```sh
python3 examples/uhp/terminal/consumer.py --fixtures
```

To query an explicitly selected development endpoint without performing a
mutation:

```sh
python3 examples/uhp/terminal/consumer.py --socket ~/.luvus-dev/luvus.sock
```

Or let the example run the exact discovery-only command and inspect every
running Unix session:

```sh
python3 examples/uhp/terminal/consumer.py --discover
```

The example sends only capability and inventory reads. Never point mutation
tests at an installed production session. A live CI conformance run must use a
dedicated `LUVUS_HOME` and terminals created for that run.

Run the fixture-backed mock and example consumer together:

```sh
python3 examples/uhp/terminal/mock_conformance.py
```

After building the debug binary, run the live lifecycle suite. It creates its
temporary `LUVUS_HOME` only below this checkout's `target/` directory:

```sh
cargo build --locked
python3 examples/uhp/terminal/live_conformance.py
```

The live suite negotiates 1.0, rejects an incompatible version, inspects cached
process identity, subscribes before create, checks lifecycle events, uses
`wait_output` instead of polling capture, verifies the fenced snapshot, and
closes only the terminal it created.

Run the deterministic failure-injection suite against the same debug binary:

```sh
python3 examples/uhp/terminal/failure_conformance.py \
  --luvus target/debug/luvus
```

It starts its own server and verifies missing and oversized frames, duplicate
keys, one-frame connection semantics, stale generations and routes, timeout,
lost-response reconciliation, subscriber capacity, close cancellation,
endpoint replacement, and server restart. Linux and macOS CI run it after live
conformance. The event queue overflow path itself remains a deterministic Rust
unit test because operating-system socket buffer sizes make a deliberately slow
live reader nondeterministic.

On Windows, build the debug binary and run the independent PowerShell consumer:

```powershell
cargo build --locked
.\examples\uhp\terminal\live_conformance.ps1 `
  -Luvus .\target\debug\luvus.exe
```

The Windows suite validates discovery of the actual local named-pipe address,
exact 1.0 negotiation, an incompatible-version rejection, ConPTY creation,
the process creation marker, process discovery, input/output, lifecycle events,
and close. CI runs fixture, mock, and live conformance on Linux, macOS, and
Windows. Every live suite keeps its isolated state below this checkout's
`target/` directory.

## Release performance benchmark

Build the optimized binary and run the isolated 1, 10, and 50-pane matrix:

```sh
cargo build --release --locked
python3 examples/uhp/terminal/benchmark.py \
  --luvus target/release/luvus \
  --panes 1,10,50 \
  --samples 50 \
  --idle-seconds 5
```

Each pane count gets a fresh server and state directory below `target/`. The
script verifies the exact inventory size and records creation, capability,
inventory, visible-capture, and input-queue latency; response sizes; idle CPU;
RSS; thread count; and descriptors. On macOS it additionally records physical
footprint, peak footprint, and live malloc bytes using the operating-system
tools.

The benchmark is intentionally opt-in rather than a shared-runner pass/fail
gate. CPU and memory results depend on the host, shell, allocator, filesystem,
and scheduler. Compare release runs only when their source, binary digest,
machine, pane workload, sample count, and idle window match. The JSON report
records those details and its measurement caveats. Use `--output` only with a
path inside the current checkout when a retained machine-readable artifact is
needed; generated results are not normative protocol files.
