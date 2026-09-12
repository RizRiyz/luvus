# Graphics delivery benchmark

`graphics_benchmark.py` runs an isolated release server and a synthetic receiver
for binary display protocol **18**. It creates real PTYs in a visible workspace,
with 1, 2, 4, or 8 panes. It never selects an inherited production socket.
Temporary server state and producer sockets live under this checkout's `target/`
and are removed after each case. The supplied binary is not installed or signed
by this harness.

## Run

Run from the repository root on macOS or Linux, with Python 3.9+:

```sh
cargo build --release --locked

# Inline RGB, 64 chunks per 256x256 image, concurrent panes.
python3 examples/uhp/terminal/graphics_benchmark.py \
  --side 256 --samples 20 --idle-seconds 5 \
  --output target/graphics-256-concurrent.json

# One-chunk images: a control for multi-chunk transfer failures.
python3 examples/uhp/terminal/graphics_benchmark.py \
  --side 32 --samples 20 --idle-seconds 5 \
  --output target/graphics-32-concurrent.json

# Same multi-pane layout, but finish each delivery before starting the next.
python3 examples/uhp/terminal/graphics_benchmark.py \
  --side 256 --samples 20 --idle-seconds 5 --mode serial \
  --output target/graphics-256-serial.json

# Four placements in one pane, refreshed with fresh IDs on every sample.
python3 examples/uhp/terminal/graphics_benchmark.py \
  --panes 1 --images 4 --side 256 --samples 10 --idle-seconds 5 \
  --output target/graphics-256-four-images.json

# Real client fallback, with a PTY that never answers terminal probes.
python3 examples/uhp/terminal/graphics_benchmark.py \
  --plain-smoke --output target/graphics-plain-smoke.json
```

Run benchmark processes **sequentially**, with other machine activity kept as
stable as practical. Do not modify the harness during a run: producer processes
launch the same script. Each report records the Git revision, working-tree
status, binary SHA-256, harness SHA-256, and OS. A binary digest identifies the
binary; it does not prove that an arbitrary `--luvus` binary was built from the
recorded checkout. Build it immediately before measuring.

## Workload and validation

- Synthetic viewport: 240×80 cells, 8×16 pixels per cell. Placements are 8×4
  cells, with multiple placements spaced vertically in a pane.
- Inline uncompressed RGB (`f=24,t=d`), deterministic pixels, Base64 payloads
  split into at most 4096 bytes. Only the first chunk carries image metadata;
  continuation chunks carry `m` and `q`.
- Each round uses fresh globally unique image IDs; this exercises retained-image
  growth/eviction, rather than replacement of a fixed image ID. This is not a
  fixed-frame-rate animation test.
- Two warm-up samples precede the measured samples. With `--images 4`, a sample
  contains four delivery rounds, one for each placement slot.
- Concurrent mode starts one producer per pane using a thread pool, then waits
  for every image. Serial mode waits for one pane before triggering the next.
- The receiver requires complete non-interleaved image transfers and validates
  the decoded payload's SHA-256 against the source. A failure is recorded as
  `status: failed`, without misleading latency/CPU figures for that case. Other
  cases continue; the process exits 1 if any case failed.
- Plain smoke starts the real release client in a 120×40 PTY. It checks graphics
  unavailability, a visible text marker after attempted image output, absence
  of forwarded image commands/placeholders, and capture of the marker.

The non-interleaving requirement follows the Kitty protocol's
[remote-client transmission rules](https://sw.kovidgoyal.net/kitty/graphics-protocol/#remote-client):
all chunks of one image must finish before another graphics command starts.

## Interpretation

`delivery` is elapsed monotonic time from just before the producer's first PTY
write to receipt of the complete image in the synthetic receiver. The receive
timestamp precedes payload verification. `batch_delivery` spans the earliest
producer start to the latest completion in the round; in serial mode it
includes the sequential waits.

CPU and RSS describe the **server only**. Active CPU includes warm-up; baseline
and post-image idle measurements use the configured idle window after a
one-second settling interval. macOS `ps` CPU time has coarse resolution, and RSS
is residency, not live heap or physical footprint. Short trials and RSS deltas
do not establish absence of a memory leak.

These measurements exclude the real graphics-capable Luvus client, terminal
image decoding, GPU rendering, compositor, and physical display. They do not
measure time-to-visible-image or FPS. The plain PTY smoke does not substitute
for manual Terminal.app, Ghostty, Kitty, or Linux desktop-terminal testing.
No PNG, file/shared-memory transport, remote attachment, slow receiver, or
cross-platform graphics rendering is validated by this benchmark.
