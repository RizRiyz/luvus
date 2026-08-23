#!/usr/bin/env python3
"""Exercise the example consumer against the fixture-driven mock server."""

import pathlib
import subprocess
import sys
import tempfile
import time

from consumer import inspect_endpoint

ROOT = pathlib.Path(__file__).resolve().parents[2]


def main():
    target = ROOT / "target"
    target.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="terminal-backend-mock-", dir=target) as state:
        state_path = pathlib.Path(state)
        state_path.chmod(0o700)
        socket_path = state_path / "backend.sock"
        server = subprocess.Popen(
            [sys.executable, str(ROOT / "examples/terminal-backend/mock_server.py"), "--socket", str(socket_path), "--requests", "2"],
            cwd=ROOT,
        )
        try:
            deadline = time.monotonic() + 5
            while not socket_path.exists():
                if server.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("terminal backend mock did not start")
                time.sleep(0.01)
            inspected = inspect_endpoint(socket_path, "fixture-mock")
            assert inspected["capabilities"]["result"]["protocol"] == {
                "name": "luvus-terminal-backend", "major": 1, "minor": 0
            }
            if server.wait(timeout=3) != 0:
                raise RuntimeError("terminal backend mock failed")
            print("terminal-backend mock conformance passed")
        finally:
            if server.poll() is None:
                server.terminate()
                server.wait(timeout=3)


if __name__ == "__main__":
    main()
