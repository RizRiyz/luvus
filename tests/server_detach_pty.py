"""Real Unix server/client lifecycle regressions for issue #417.

Run: python3 tests/server_detach_pty.py --luvus target/debug/luvus
All homes, sockets and PTYs are isolated under this checkout's target directory.
No installed Luvus binary, production session or real agent is used.
"""

import argparse
from contextlib import contextmanager
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import time
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
STATE_ROOT = ROOT / "target"
TIMEOUT = 12
BINARY = ROOT / "target/debug/luvus"


def process_table():
    # FreeBSD treats everything after '=' as the heading, including commas.
    output = subprocess.check_output(["ps", "-ax", "-o", "pid=", "-o", "ppid="], text=True)
    return {int(pid): int(parent) for pid, parent in
            (line.split() for line in output.splitlines() if line.strip())}


def descendants(root, table):
    found = [root]
    for parent in found:
        found.extend(pid for pid, ppid in table.items() if ppid == parent and pid not in found)
    return found


def wait_until(check, message, timeout=TIMEOUT):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if check():
            return
        time.sleep(0.03)
    raise AssertionError(message)


def receive_reply(control):
    data = b""
    while len(data) < 8:
        part = control.recv(8 - len(data))
        assert part, "launcher closed its control channel"
        data += part
    value, error = struct.unpack("<ii", data)
    assert error == 0, (value, error)
    return value


class ProcessTableTests(unittest.TestCase):
    def test_separate_headerless_columns_are_portable(self):
        with mock.patch.object(subprocess, "check_output", return_value="\n 11 1\n 22 11\n") as ps:
            self.assertEqual(process_table(), {11: 1, 22: 11})
        ps.assert_called_once_with(["ps", "-ax", "-o", "pid=", "-o", "ppid="], text=True)

    def test_missing_parent_column_is_not_silently_ignored(self):
        with mock.patch.object(subprocess, "check_output", return_value="11\n"):
            with self.assertRaises(ValueError):
                process_table()


class DetachedServerTests(unittest.TestCase):
    def setUp(self):
        STATE_ROOT.mkdir(parents=True, exist_ok=True)
        self.home = Path(tempfile.mkdtemp(prefix="i417-", dir=STATE_ROOT))
        self.env = {key: value for key, value in os.environ.items() if not key.startswith("LUVUS_")}
        self.env.update(LUVUS_HOME=str(self.home), TERM="xterm-256color", SHELL="/bin/sh")
        (self.home / "config.json").write_text(json.dumps({"check_updates": False, "theme": "quattro-rally"}))
        self.sessions = set()
        self.processes = []
        self.clients = []
        self.helpers = []
        self.masters = []

    def command(self, args, **kwargs):
        if args[:1] == ["--session"]:
            self.sessions.add(args[1])
        process = subprocess.Popen([str(BINARY), *args], env=self.env, cwd=self.home, **kwargs)
        self.processes.append(process)
        return process

    def cli(self, args, success=True):
        process = self.command(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        output, error = process.communicate(timeout=TIMEOUT)
        if success:
            self.assertEqual(process.returncode, 0, (args, output, error))
        else:
            self.assertNotEqual(process.returncode, 0, (args, output, error))
        return output, error

    def session_dir(self, name):
        return self.home if name == "default" else self.home / "sessions" / name

    def server_pid(self, name):
        return int((self.session_dir(name) / "server.pid").read_text().split()[0])

    def inventory(self, name):
        with socket.socket(socket.AF_UNIX) as control:
            control.settimeout(TIMEOUT)
            control.connect(str(self.session_dir(name) / "luvus.sock"))
            request = {"id": "detach-test", "method": "terminal.backend.inventory", "params": {}}
            control.sendall((json.dumps(request) + "\n").encode())
            with control.makefile("rb") as reader:
                value = json.loads(reader.readline())
        self.assertNotIn("error", value, value)
        return [(item["terminal_id"], item["root_process"]) for item in value["result"]["terminals"]]

    def drain(self, master, duration=0.2):
        output = b""
        deadline = time.monotonic() + duration
        while time.monotonic() < deadline:
            if select.select([master], [], [], max(0, deadline - time.monotonic()))[0]:
                try:
                    part = os.read(master, 65536)
                except OSError:
                    break
                if not part:
                    break
                output += part
        return output

    def client(self, name, supervised=False):
        self.sessions.add(name)
        master, slave = pty.openpty()
        self.masters.append(master)
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
        args = [str(BINARY), "--session", name]
        if supervised:
            # Keep the original launcher alive. Killing its descendants is the
            # reported supervisor cleanup, unlike merely closing the client.
            args = [sys.executable, "-c", "import subprocess,sys; sys.exit(subprocess.call(sys.argv[1:]))", *args]
        try:
            process = subprocess.Popen(args, env=self.env, cwd=self.home,
                                       stdin=slave, stdout=slave, stderr=slave, start_new_session=True)
        finally:
            os.close(slave)
        self.clients.append(process)
        output = bytearray()

        def ready():
            output.extend(self.drain(master, 0.05))
            self.assertIsNone(process.poll(), bytes(output))
            return b"WORKSPACES" in output

        wait_until(ready, f"client never entered its terminal: {bytes(output)!r}")
        self.assertTrue(self.inventory(name), "server did not create a real PTY")
        return process, master

    def helper(self, name):
        control, helper = socket.socketpair()
        control.settimeout(TIMEOUT)
        try:
            process = self.command(["--session", name, "__server-launch-helper"],
                                   stdin=helper, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        finally:
            helper.close()
        self.helpers.append((process, control))
        return process, control, receive_reply(control)

    @contextmanager
    def stalled_restore(self, name, args):
        """Block the real snapshot reader after socket binding, without a test-only binary hook."""
        folder = self.session_dir(name)
        folder.mkdir(parents=True, exist_ok=True)
        snapshot = folder / "session.json"
        os.mkfifo(snapshot)
        process = self.command(["--session", name, *args],
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        writers = []

        def reader_started():
            try:
                writers.append(os.open(snapshot, os.O_WRONLY | os.O_NONBLOCK))
                return True
            except OSError as error:
                if error.errno == errno.ENXIO:
                    return False
                raise

        try:
            wait_until(reader_started, "server did not enter snapshot restoration")
            records = [json.loads(line) for line in (folder / "logs/server.log").read_text().splitlines()]
            pid = next(record["pid"] for record in records if record["event"] == "server.start")
            yield process, pid
        finally:
            for writer in writers:
                try:
                    os.write(writer, b"{}")
                except BrokenPipeError:
                    pass
                finally:
                    os.close(writer)
            snapshot.unlink(missing_ok=True)

    def tearDown(self):
        for process, control in self.helpers:
            control.close()
            try:
                process.communicate(timeout=TIMEOUT)
            except subprocess.TimeoutExpired:
                process.kill()
                process.communicate(timeout=TIMEOUT)
        for process in self.clients:
            if process.poll() is None:
                # Only this fixture's still-owned supervisor/client tree.
                for pid in reversed(descendants(process.pid, process_table())):
                    try:
                        os.kill(pid, signal.SIGTERM)
                    except ProcessLookupError:
                        pass
                process.wait(timeout=TIMEOUT)
        for name in self.sessions:
            subprocess.run([str(BINARY), "--session", name, "server", "stop"],
                           env=self.env, cwd=self.home, stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL, timeout=TIMEOUT)
        for process in self.processes:
            if process.poll() is None:
                process.kill()
            process.communicate(timeout=TIMEOUT)
        for master in self.masters:
            os.close(master)
        shutil.rmtree(self.home)

    def test_client_tree_kill_preserves_default_and_named_server_and_pty(self):
        for name in ("default", "tree-probe"):
            with self.subTest(session=name):
                supervisor, _ = self.client(name, supervised=True)
                pid = self.server_pid(name)
                before = self.inventory(name)
                tree = descendants(supervisor.pid, process_table())
                self.assertGreater(len(tree), 1, "supervisor did not own a client")
                for child in reversed(tree):
                    try:
                        os.kill(child, signal.SIGTERM)
                    except ProcessLookupError:
                        # The supervisor may exit as soon as its client dies.
                        pass
                supervisor.wait(timeout=TIMEOUT)
                self.cli(["--session", name, "ping"])
                self.assertNotIn(pid, tree, "server remained in the client's descendant tree")
                self.assertEqual(self.server_pid(name), pid)
                self.assertEqual(self.inventory(name), before, "pane/PTY lifetime changed")

    def test_normal_detach_and_reattach_preserve_the_server_and_pty(self):
        client, master = self.client("detach")
        before = self.inventory("detach")
        pid = self.server_pid("detach")
        os.write(master, b"\x00q")
        self.drain(master)
        self.assertEqual(client.wait(timeout=TIMEOUT), 0)
        second, second_master = self.client("detach")
        self.assertEqual(self.server_pid("detach"), pid)
        self.assertEqual(self.inventory("detach"), before)
        os.write(second_master, b"\x00q")
        self.drain(second_master)
        self.assertEqual(second.wait(timeout=TIMEOUT), 0)

    def test_named_start_stop_restart_and_restart_all_keep_state_isolated(self):
        for name in ("alpha", "beta", "stopped"):
            self.cli(["--session", name, "server", "start"])
            self.cli(["--session", name, "workspace", "rename", "0", name + "-only"])
        self.cli(["session", "stop", "stopped"])
        self.cli(["--session", "alpha", "server", "restart"])
        self.assertTrue(self.inventory("alpha"))
        before = {name: self.server_pid(name) for name in ("alpha", "beta")}
        self.cli(["--session", "alpha", "server", "restart", "--all"])
        for name in before:
            self.assertNotEqual(self.server_pid(name), before[name])
            output, _ = self.cli(["--session", name, "workspace", "list"])
            self.assertIn((name + "-only").encode(), output)
            self.assertTrue(self.inventory(name))
        output, _ = self.cli(["session", "list", "--json"])
        listed = {item["name"]: item["running"] for item in json.loads(output)["sessions"]}
        self.assertFalse(listed["stopped"])
        self.cli(["session", "stop", "alpha"])
        self.cli(["--session", "alpha", "server", "start"])
        output, _ = self.cli(["--session", "alpha", "workspace", "list"])
        self.assertIn(b"alpha-only", output)

    def test_simultaneous_starts_reuse_one_server(self):
        for index in range(3):
            name = f"race-{index}"
            processes = [self.command(["--session", name, "server", "start"],
                                      stdout=subprocess.PIPE, stderr=subprocess.PIPE) for _ in range(3)]
            for process in processes:
                output, error = process.communicate(timeout=TIMEOUT)
                self.assertEqual(process.returncode, 0, (output, error))
            self.assertEqual(len(self.inventory(name)), 1)

    def test_slow_automatic_restore_survives_and_can_attach_later(self):
        for name in ("default", "slow-restore"):
            with self.subTest(session=name):
                with self.stalled_restore(name, ["server", "start"]) as (process, pid):
                    output, error = process.communicate(timeout=TIMEOUT)
                    self.assertEqual(process.returncode, 0, (output, error))
                    # Deliberately exceed the five-second readiness deadline
                    # while the real server remains blocked in snapshot I/O.
                    time.sleep(6)
                    self.assertIn(pid, process_table(), "slow automatic startup was killed")
                before = self.inventory(name)
                self.assertEqual(self.server_pid(name), pid)
                client, master = self.client(name)
                self.assertEqual(self.inventory(name), before)
                os.write(master, b"\x00q")
                self.drain(master)
                self.assertEqual(client.wait(timeout=TIMEOUT), 0)

    def test_slow_managed_restore_keeps_its_bound_server(self):
        name = "slow-managed"
        with self.stalled_restore(name, ["web", "--no-open", "--port", "0"]) as (process, pid):
            time.sleep(6)
            table = process_table()
            self.assertIn(pid, table, "managed readiness timeout killed a bound server")
            self.assertNotIn(pid, descendants(process.pid, table))
            if process.poll() is None:
                process.terminate()
            process.communicate(timeout=TIMEOUT)
        self.assertTrue(self.inventory(name))
        self.assertEqual(self.server_pid(name), pid)

    def test_named_selector_creates_an_independent_detached_server(self):
        client, master = self.client("source")
        before = self.inventory("source")
        source_pid = self.server_pid("source")
        self.sessions.add("ui-new")
        os.write(master, b"\x00t")
        menu_output = bytearray()

        def menu_ready():
            menu_output.extend(self.drain(master, 0.05))
            self.assertIsNone(client.poll(), bytes(menu_output))
            return b"New Session" in menu_output

        wait_until(menu_ready, "named-session menu did not load")
        os.write(master, b"k\rui-new\r")  # current row -> New session -> submit
        output = bytearray()

        def switched():
            output.extend(self.drain(master, 0.05))
            self.assertIsNone(client.poll(), bytes(output))
            return "luvus · ui-new".encode() in output

        wait_until(switched, "selector did not create and attach the new session")
        # The terminal title is sent before handoff's first interactive frame.
        # Wait for a destination-only UI change instead of guessing a delay.
        # All-new cells keep the marker contiguous even in a sparse frame diff.
        self.cli(["--session", "ui-new", "workspace", "rename", "0", "QQQQQQQQQ"])

        def destination_frame():
            output.extend(self.drain(master, 0.05))
            return b"QQQQQQQQQ" in output

        wait_until(destination_frame, "destination frame did not arrive after session handoff")
        self.assertEqual(self.inventory("source"), before)
        self.assertNotEqual(self.inventory("ui-new"), before)
        self.assertNotIn(self.server_pid("ui-new"), descendants(source_pid, process_table()))
        os.write(master, b"\x00q")
        self.drain(master)
        self.assertEqual(client.wait(timeout=TIMEOUT), 0)

    def test_explicit_termination_signals_still_stop_and_save_the_server(self):
        for index, termination in enumerate((signal.SIGTERM, signal.SIGHUP, signal.SIGINT)):
            with self.subTest(signal=termination):
                name = f"signal-{index}"
                self.cli(["--session", name, "server", "start"])
                self.cli(["--session", name, "workspace", "rename", "0", "saved-by-signal"])
                pid = self.server_pid(name)
                os.kill(pid, termination)
                wait_until(lambda: pid not in process_table(), "explicit signal did not stop server")
                self.cli(["--session", name, "server", "start"])
                output, _ = self.cli(["--session", name, "workspace", "list"])
                self.assertIn(b"saved-by-signal", output)

    def test_bare_server_remains_a_foreground_owned_process(self):
        process = self.command(["--session", "foreground", "server"],
                               stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                               stderr=subprocess.PIPE)

        def ready():
            if process.poll() is not None:
                _, error = process.communicate(timeout=TIMEOUT)
                self.fail(f"foreground server exited before startup: {error!r}")
            return (self.session_dir("foreground") / "server.pid").exists()

        wait_until(ready, "foreground server did not start")
        self.assertEqual(self.server_pid("foreground"), process.pid)
        self.assertEqual(process_table()[process.pid], os.getpid())
        process.terminate()
        self.assertEqual(process.wait(timeout=TIMEOUT), 0)

    def test_failed_start_reports_exit_without_leaking_a_launcher(self):
        folder = self.session_dir("failure")
        folder.mkdir(parents=True)
        (folder / "luvus.sock").write_text("do not replace a regular file")
        _, error = self.cli(["--session", "failure", "web", "--no-open", "--port", "0"], success=False)
        self.assertIn(b"exited before startup", error)
        self.assertEqual((folder / "luvus.sock").read_text(), "do not replace a regular file")

    def test_startup_timeout_kills_and_reaps_only_its_owned_children(self):
        folder = self.session_dir("timeout")
        folder.mkdir(parents=True)
        with (folder / "server.lock").open("w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            process = self.command(["--session", "timeout", "web", "--no-open", "--port", "0"],
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            tree = []

            def started():
                tree[:] = descendants(process.pid, process_table())[1:]
                return len(tree) >= 2

            wait_until(started, "launcher did not spawn its blocked server")
            _, error = process.communicate(timeout=TIMEOUT)
            self.assertNotEqual(process.returncode, 0)
            self.assertIn(b"did not start within", error)
            wait_until(lambda: all(pid not in process_table() for pid in tree), "startup children leaked")
        self.cli(["--session", "timeout", "server", "start"])

    def test_helper_cancellation_parent_eof_and_invalid_request_reap_server(self):
        for index, request in enumerate((b"C", None, b"X")):
            with self.subTest(request=request):
                process, control, pid = self.helper(f"cancel-{index}")
                if request is None:
                    control.close()
                else:
                    control.sendall(request)
                    if request == b"C":
                        self.assertEqual(receive_reply(control), 0)
                process.communicate(timeout=TIMEOUT)
                wait_until(lambda: pid not in process_table(), "unreleased server leaked")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--luvus", type=Path, default=BINARY)
    parser.add_argument("--state-root", type=Path, default=STATE_ROOT,
                        help="fixture directory under target (use a native filesystem in Docker)")
    options, rest = parser.parse_known_args()
    BINARY = options.luvus.resolve(strict=True)
    STATE_ROOT = options.state_root.resolve()
    if not STATE_ROOT.is_relative_to(ROOT / "target"):
        parser.error("--state-root must remain under this checkout's target directory")
    print(f"Testing {BINARY}; isolated homes under {STATE_ROOT}", flush=True)
    unittest.main(argv=[sys.argv[0], *rest], verbosity=2)
