"""Unix --local focus-redraw regression; never contacts a production server.

Run: python3 scripts/test-local-focus-pty.py --luvus target/debug/luvus
The host PTY intentionally does not answer cursor queries. Focus recovery must
repaint without querying the cursor or exiting while the input thread runs.
"""

import argparse
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import struct
import subprocess
import tempfile
import termios
import time


ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--luvus", type=Path, default=ROOT / "target/debug/luvus")
    binary = parser.parse_args().luvus.resolve(strict=True)
    (ROOT / "target").mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="local-focus-", dir=ROOT / "target") as folder:
        home = Path(folder)
        (home / "config.json").write_text(json.dumps({
            "check_updates": False, "shell": "/bin/sh", "theme": "quattro-rally",
        }))
        env = {key: value for key, value in os.environ.items()
               if not key.startswith("LUVUS_")}
        env.update(LUVUS_HOME=folder, TERM="xterm-256color", RUST_BACKTRACE="1")
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        with tempfile.TemporaryFile() as errors:
            try:
                process = subprocess.Popen([str(binary), "--local"], cwd=ROOT, env=env,
                                           stdin=slave, stdout=slave, stderr=errors,
                                           start_new_session=True)
            finally:
                os.close(slave)
            output = bytearray()

            def pump(timeout=0.05):
                if select.select([master], [], [], timeout)[0]:
                    try:
                        output.extend(os.read(master, 65536))
                    except OSError:
                        pass

            def assert_alive():
                if process.poll() is not None:
                    errors.seek(0)
                    raise AssertionError(
                        f"local client exited with {process.returncode}: "
                        + errors.read().decode(errors="replace"))

            try:
                deadline = time.monotonic() + 10
                while b"WORKSPACES" not in output:
                    pump()
                    assert_alive()
                    assert time.monotonic() < deadline, "local client did not render"

                # No split/transpose needed: focus recovery alone used to exit.
                output.clear()
                os.write(master, b"\x1b[I")
                deadline = time.monotonic() + 3
                while time.monotonic() < deadline:
                    pump()
                    assert_alive()
                assert b"\x1b[6n" not in output, "redraw queried the host cursor"
                assert b"WORKSPACES" in output, "focus recovery did not fully repaint"

                # Exercise the reported feature path with a real terminal input
                # stream, then focus recovery again. Wait for each resulting frame
                # instead of sleeping for a fixed duration on loaded CI hosts.
                for chord in (b"\x00v", b"\x00T", b"\x00T", b"\x00s", b"\x00T"):
                    before = len(output)
                    os.write(master, chord)
                    deadline = time.monotonic() + 3
                    while len(output) == before and time.monotonic() < deadline:
                        pump()
                        assert_alive()
                    assert len(output) > before, "chord produced no observable redraw"
                os.write(master, b"\x1b[I")
                deadline = time.monotonic() + 3
                before = len(output)
                while len(output) == before and time.monotonic() < deadline:
                    pump()
                    assert_alive()
                assert len(output) > before, "focus recovery produced no observable redraw"
                assert b"\x1b[6n" not in output, "split/transpose queried the host cursor"

                os.write(master, b"\x00q")
                deadline = time.monotonic() + 5
                while process.poll() is None and time.monotonic() < deadline:
                    pump()
                assert process.poll() == 0, "local client did not detach cleanly"
                print(f"PASS: {binary}; focus repaint, split/transpose, clean detach; isolated home")
            finally:
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)
                os.close(master)


if __name__ == "__main__":
    main()
