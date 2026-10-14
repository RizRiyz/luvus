"""Real Unix client/server keyboard-forwarding regression.

Run: python3 scripts/test-keyboard-forwarding-pty.py --luvus target/debug/luvus
Uses isolated homes under target and a raw PTY byte recorder, not a real agent
or production session. Native Windows console input requires Windows testing.
"""

import argparse
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import shlex
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import time
import tty
import unittest


ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "target/debug/luvus"
TIMEOUT = 8


def probe(flags, output):
    """Negotiate child keyboard modes and record actual bytes from its PTY."""
    fd = sys.stdin.fileno()
    assert os.isatty(fd), "probe must run in a real PTY"
    saved = termios.tcgetattr(fd)
    try:
        tty.setraw(fd)
        with Path(output).open("wb", buffering=0) as capture:
            os.write(sys.stdout.fileno(),
                     f"\x1b[>{flags}u\x1b[?2004h\r\nKEYBOARD_PROBE_READY\r\n".encode())
            # A bounded idle lifetime also prevents an orphaned test recorder.
            while select.select([fd], [], [], 30)[0]:
                data = os.read(fd, 4096)
                if not data:
                    break
                capture.write(data)
    finally:
        os.write(sys.stdout.fileno(), b"\x1b[<u\x1b[?2004l")
        termios.tcsetattr(fd, termios.TCSANOW, saved)


class KeyboardForwardingTests(unittest.TestCase):
    def setUp(self):
        (ROOT / "target").mkdir(exist_ok=True)
        fixture = tempfile.TemporaryDirectory(prefix="keys-", dir=ROOT / "target")
        self.addCleanup(fixture.cleanup)
        self.home = Path(fixture.name)
        self.env = {k: v for k, v in os.environ.items() if not k.startswith("LUVUS_")}
        self.env.update(LUVUS_HOME=str(self.home), TERM="xterm-256color", SHELL="/bin/sh")
        (self.home / "config.json").write_text(json.dumps({
            "check_updates": False, "shell": "/bin/sh", "theme": "quattro-rally",
        }))
        self.socket = self.home / "sessions" / "keys" / "luvus.sock"
        self.output = self.home / "input.bin"
        self.master = None
        self.terminal_output = bytearray()
        server = subprocess.Popen(
            [str(BINARY), "--session", "keys", "server"],
            env=self.env, cwd=self.home, stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        self.addCleanup(self.stop, server)

        def ready():
            self.assertIsNone(server.poll(), "isolated server exited")
            try:
                return bool(self.api("ping"))
            except (OSError, KeyError):
                return False

        self.wait_until(ready, "isolated server did not become ready")
        master, slave = pty.openpty()
        self.master = master
        self.addCleanup(os.close, master)
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
        try:
            client = subprocess.Popen(
                [str(BINARY), "--session", "keys", "client"],
                env=self.env, cwd=self.home, stdin=slave, stdout=slave,
                stderr=slave, start_new_session=True,
            )
        finally:
            os.close(slave)
        self.addCleanup(self.stop, client)
        self.wait_until(lambda: b"WORKSPACES" in self.terminal_output,
                        "isolated client did not render")
        print(f"\nBinary: {BINARY}\nHome: {self.home}\nSession: keys\nSocket: {self.socket}")

    @staticmethod
    def stop(process):
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=TIMEOUT)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=TIMEOUT)

    def drain(self, timeout=0.03):
        if self.master is not None and select.select([self.master], [], [], timeout)[0]:
            self.terminal_output.extend(os.read(self.master, 65536))
            del self.terminal_output[:-262144]

    def wait_until(self, check, message):
        deadline = time.monotonic() + TIMEOUT
        while time.monotonic() < deadline:
            self.drain()
            if check():
                return
            if self.master is None:
                time.sleep(0.03)
        self.fail(f"{message}; terminal tail: {bytes(self.terminal_output[-2000:])!r}")

    def api(self, method, params=None):
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(1)
            connection.connect(str(self.socket))
            request = {"id": "keyboard-test", "method": method, "params": params or {}}
            connection.sendall((json.dumps(request) + "\n").encode())
            with connection.makefile("rb") as reply:
                value = json.loads(reply.readline())
        self.assertNotIn("error", value, value)
        return value["result"]

    def check_protocol(self, flags):
        command = shlex.join([sys.executable, str(Path(__file__).resolve()),
                              "--probe", str(flags), str(self.output)])
        # Launch without leaving a host Enter press owned by the shell when
        # the child switches protocols. Every assertion below uses TUI input.
        self.api("pane.run", {"command": command})
        self.wait_until(lambda: "KEYBOARD_PROBE_READY" in self.api("pane.read")["text"],
                        "child keyboard negotiation did not reach the server")
        extended = bool(flags & (1 | 8))
        report_all = bool(flags & 8)
        events = bool(flags & 2) and extended
        cases = [
            ("Enter", b"\r", b"\x1b[13u" if report_all else b"\r"),
            ("Enter release", b"\x1b[13;1:3u", b"\x1b[13;1:3u" if report_all and events else b""),
            ("Ctrl+Enter", b"\x1b[13;5u", b"\x1b[13;5u" if extended else b"\r"),
            ("Ctrl+Enter repeat", b"\x1b[13;5:2u",
             b"\x1b[13;5:2u" if events else b"\x1b[13;5u" if extended else b"\r"),
            ("Ctrl+Enter release", b"\x1b[13;5:3u", b"\x1b[13;5:3u" if events else b""),
            ("Ctrl+Shift+Enter", b"\x1b[13;6u", b"\x1b[13;6u" if extended else b"\x1b\r"),
            ("Ctrl+Shift+Enter release", b"\x1b[13;6:3u", b"\x1b[13;6:3u" if events else b""),
            ("Shift+Enter", b"\x1b[13;2u", b"\x1b[13;2u" if extended else b"\x1b\r"),
            ("Shift+Enter release", b"\x1b[13;2:3u", b"\x1b[13;2:3u" if events else b""),
            ("Alt+Enter", b"\x1b[13;3u", b"\x1b[13;3u" if extended else b"\x1b\r"),
            ("Alt+Enter release", b"\x1b[13;3:3u", b"\x1b[13;3:3u" if events else b""),
            ("Ctrl+C", b"\x03", b"\x1b[99;5u" if extended else b"\x03"),
            ("Ctrl+C release", b"\x1b[99;5:3u", b"\x1b[99;5:3u" if events else b""),
            ("Ctrl+Left", b"\x1b[1;5D", b"\x1b[1;5D"),
            ("Ctrl+Left release", b"\x1b[1;5:3D", b"\x1b[1;5:3D" if flags & 2 else b""),
            ("Alt+Backspace", b"\x1b\x7f", b"\x1b[127;3u" if report_all else b"\x1b\x7f"),
            ("Alt+Backspace release", b"\x1b[127;3:3u", b"\x1b[127;3:3u" if report_all and events else b""),
            ("Tab", b"\t", b"\x1b[9u" if report_all else b"\t"),
            ("Tab release", b"\x1b[9;1:3u", b"\x1b[9;1:3u" if report_all and events else b""),
            ("text", b"a", b"\x1b[97u" if report_all else b"a"),
            ("text release", b"\x1b[97;1:3u", b"\x1b[97;1:3u" if report_all and events else b""),
            ("paste", b"\x1b[200~hello\nworld\x1b[201~",
             b"\x1b[200~hello\nworld\x1b[201~"),
        ]
        expected = b""
        # A paste fence uses the same client/PTY input stream without creating
        # held-key ownership. Its capture proves preceding input was processed,
        # including releases that should produce no bytes.
        fence = b"\x1b[200~KEYBOARD_INPUT_FENCE\x1b[201~"
        for name, sent, received in cases:
            with self.subTest(key=name, flags=flags):
                os.write(self.master, sent + fence)
                expected += received + fence
                self.wait_until(lambda: len(self.output.read_bytes()) >= len(expected),
                                f"{name} input fence was not delivered")
                self.assertEqual(self.output.read_bytes(), expected, f"{name}, flags={flags}")

    def test_disambiguation(self):
        self.check_protocol(1)

    def test_disambiguation_with_events(self):
        self.check_protocol(3)

    def test_report_all_with_events(self):
        self.check_protocol(10)

    def test_legacy(self):
        self.check_protocol(0)

    def test_event_reporting_without_disambiguation(self):
        self.check_protocol(2)


if __name__ == "__main__":
    if sys.argv[1:2] == ["--probe"]:
        probe(int(sys.argv[2]), sys.argv[3])
    else:
        parser = argparse.ArgumentParser(description=__doc__)
        parser.add_argument("--luvus", type=Path, default=BINARY)
        args, remaining = parser.parse_known_args()
        BINARY = args.luvus.resolve(strict=True)
        unittest.main(argv=[sys.argv[0], *remaining], verbosity=2)
