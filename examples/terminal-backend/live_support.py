#!/usr/bin/env python3
"""Shared isolation helpers for live terminal-backend development tests."""

from contextlib import contextmanager
import os
import pathlib
import socket
import subprocess
import tempfile
import time


ROOT = pathlib.Path(__file__).resolve().parents[2]


def isolated_environment(state):
    environment = os.environ.copy()
    environment["LUVUS_HOME"] = str(state)
    for key in (
        "LUVUS_SOCKET_PATH",
        "LUVUS_SESSION",
        "LUVUS_ENV",
        "LUVUS_PANE_ID",
        "BOHAY_SOCKET_PATH",
        "BOHAY_SESSION",
        "BOHAY_ENV",
        "BOHAY_PANE_ID",
        "BOHAY_HOME",
    ):
        environment.pop(key, None)
    return environment


def socket_reachable(path):
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
            stream.settimeout(0.1)
            stream.connect(str(path))
        return True
    except OSError:
        return False


class IsolatedServer:
    def __init__(self, binary, state):
        self.binary = pathlib.Path(binary).resolve(strict=True)
        self.state = pathlib.Path(state)
        self.socket_path = self.state / "luvus.sock"
        self.environment = isolated_environment(self.state)
        self.process = None

    def start(self, previous_evidence=None):
        if self.process is not None:
            raise RuntimeError("isolated Luvus server is already running")
        self.process = subprocess.Popen(
            [str(self.binary), "server"],
            cwd=ROOT,
            env=self.environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise RuntimeError("isolated Luvus server exited during startup")
            try:
                info = self.socket_path.lstat()
                evidence = (info.st_dev, info.st_ino, info.st_ctime_ns)
                if evidence != previous_evidence and socket_reachable(self.socket_path):
                    return self.process
            except FileNotFoundError:
                pass
            time.sleep(0.025)
        self.stop()
        raise RuntimeError("isolated Luvus server did not become reachable")

    def stop(self):
        process, self.process = self.process, None
        if process is None:
            return
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=3)

    def restart(self, previous_evidence=None):
        self.stop()
        return self.start(previous_evidence=previous_evidence)


@contextmanager
def isolated_server(binary, prefix):
    target = ROOT / "target"
    target.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=prefix, dir=target) as state:
        state_path = pathlib.Path(state)
        state_path.chmod(0o700)
        server = IsolatedServer(binary, state_path)
        server.start()
        try:
            yield server
        finally:
            server.stop()
