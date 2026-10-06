"""Real Unix debug-client/server regression for #479.

Run: python3 scripts/test-terminal-colors-pty.py --luvus target/debug/luvus
Uses only isolated homes and real PTYs under this checkout's target directory.
This reproduces an SSH-style environment, not an actual Windows SSH connection.
"""

import argparse
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import shlex
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import time
import unittest


ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "target/debug/luvus"
TIMEOUT = 10
RGB = re.compile(rb"(?:\x1b\[|;)38;2;(\d+);(\d+);(\d+)(?=[;m])")
INDEXED = re.compile(rb"(?:\x1b\[|;)38;5;\d+(?=[;m])")


def emit_gradient():
    assert sys.stdout.isatty(), "gradient must come from a real pane PTY"
    for row in range(16):
        line = "".join(f"\x1b[38;2;{value};{value};{value}mG"
                       for value in range(row * 16, row * 16 + 16))
        print(line + "\x1b[0m", flush=True)
    print("GRADIENT_READY", flush=True)


class TerminalColorTests(unittest.TestCase):
    def setUp(self):
        (ROOT / "target").mkdir(exist_ok=True)
        fixture = tempfile.TemporaryDirectory(prefix="c479-", dir=ROOT / "target")
        self.addCleanup(fixture.cleanup)
        self.home = Path(fixture.name)
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith("LUVUS_")
                    and key not in {"COLORTERM", "TERM_PROGRAM", "WT_SESSION", "NO_COLOR"}}
        self.env.update(LUVUS_HOME=str(self.home), TMPDIR=str(self.home),
                        TERM="xterm-256color", SHELL="/bin/sh")
        (self.home / "config.json").write_text(json.dumps({
            "check_updates": False, "shell": "/bin/sh", "theme": "quattro-rally",
        }))
        self.socket = self.home / "luvus.sock"
        server = subprocess.Popen([str(BINARY), "server"], env=self.env, cwd=self.home,
                                  stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                  stderr=subprocess.PIPE)
        self.addCleanup(self.stop, server)

        def ready():
            self.assertIsNone(server.poll(), "isolated server exited")
            try:
                return bool(self.api("ping"))
            except OSError:
                # The socket can exist before its listener accepts requests.
                return False

        self.wait_until(ready, "isolated server did not become ready")
        self.server_pid = server.pid
        self.assertEqual(int((self.home / "server.pid").read_text().split()[0]), self.server_pid)

    @staticmethod
    def stop(process):
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=TIMEOUT)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=TIMEOUT)
        if process.stderr is not None:
            process.stderr.close()

    def api(self, method, params=None):
        with socket.socket(socket.AF_UNIX) as control:
            control.settimeout(TIMEOUT)
            control.connect(str(self.socket))
            control.sendall((json.dumps({"id": "colors", "method": method,
                                         "params": params or {}}) + "\n").encode())
            with control.makefile("rb") as reader:
                reply = json.loads(reader.readline())
        self.assertNotIn("error", reply, reply)
        return reply["result"]

    def wait_until(self, check, message):
        deadline = time.monotonic() + TIMEOUT
        while time.monotonic() < deadline:
            if check():
                return
            time.sleep(0.02)
        self.fail(message)

    def attach(self, color_term=None):
        master, slave = pty.openpty()
        self.addCleanup(os.close, master)
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 28, 120, 0, 0))
        env = dict(self.env)
        if color_term is not None:
            env["COLORTERM"] = color_term
        try:
            client = subprocess.Popen([str(BINARY), "client"], env=env, cwd=self.home,
                                      stdin=slave, stdout=slave, stderr=slave,
                                      start_new_session=True)
        finally:
            os.close(slave)
        self.addCleanup(self.stop, client)
        output = bytearray()

        def ready():
            if select.select([master], [], [], 0.03)[0]:
                output.extend(os.read(master, 65536))
            self.assertIsNone(client.poll(), bytes(output))
            return b"WORKSPACES" in output

        self.wait_until(ready, "client did not render")
        return client, master, output

    def capture_gradient(self, master, output):
        def ready():
            if select.select([master], [], [], 0.03)[0]:
                output.extend(os.read(master, 65536))
            return b"GRADIENT_READY" in output

        self.wait_until(ready, "gradient did not reach the display client")
        return bytes(output)

    def detach(self, client, master):
        # The real client forwards the default detach shortcut to its server.
        os.write(master, b"\x00q")

        def exited():
            # Keep consuming the host PTY so a final full-frame write cannot
            # block the client before it processes the detach event.
            if select.select([master], [], [], 0.03)[0]:
                try:
                    os.read(master, 65536)
                except OSError:
                    pass
            return client.poll() is not None

        self.wait_until(exited, "client did not detach")
        self.assertEqual(client.returncode, 0)
        self.assertEqual(int((self.home / "server.pid").read_text().split()[0]), self.server_pid)
        self.assertTrue(self.api("ping"))

    def test_rgb_survives_missing_colorterm_and_reattach_keeps_server_colors(self):
        client, master, output = self.attach()
        command = shlex.join([sys.executable, str(Path(__file__).resolve()), "--emit"])
        self.api("pane.run", {"command": command})
        captured = self.capture_gradient(master, output)
        gray = {int(red) for red, green, blue in RGB.findall(captured) if red == green == blue}
        self.assertGreaterEqual(len(gray), 240, f"gradient was quantized: {len(gray)} gray levels")
        self.detach(client, master)

        # Same live server and terminal grid, but a client requesting 256 colors.
        client, master, output = self.attach("256color")
        captured = self.capture_gradient(master, output)
        self.assertFalse(RGB.search(captured), "limited-color client emitted RGB")
        self.assertIsNotNone(INDEXED.search(captured), "limited-color fallback was absent")
        self.detach(client, master)

        client, master, output = self.attach()
        captured = self.capture_gradient(master, output)
        gray = {int(red) for red, green, blue in RGB.findall(captured) if red == green == blue}
        self.assertGreaterEqual(len(gray), 240, "server colors were changed by the limited client")
        self.detach(client, master)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--luvus", type=Path, default=BINARY)
    parser.add_argument("--emit", action="store_true")
    options = parser.parse_args()
    if options.emit:
        emit_gradient()
    else:
        BINARY = options.luvus.resolve(strict=True)
        print(f"Testing {BINARY}; isolated homes under {ROOT / 'target'}", flush=True)
        unittest.main(argv=[sys.argv[0]], verbosity=2)
