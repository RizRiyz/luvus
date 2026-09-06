#!/usr/bin/env python3
"""Exercise agent.keys through Access against an isolated, named test server."""

import argparse
import json
import os
import pathlib
import selectors
import shlex
import socket
import subprocess
import sys
import time


def stop(process):
    if process is not None and process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


def wait_for(predicate):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.025)
    raise AssertionError("isolated fixture did not become ready")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--home", required=True)
    parser.add_argument("--session", default="luvus-pr-test")
    parser.add_argument("--expect", choices=["forbidden", "allowed"], required=True)
    args = parser.parse_args()
    binary = str(pathlib.Path(args.binary).resolve(strict=True))
    home = pathlib.Path(args.home).resolve(strict=True)
    if any(home.iterdir()):
        raise ValueError("--home must be a fresh empty test directory")
    env = os.environ.copy()
    for key in ("LUVUS_SOCKET_PATH", "LUVUS_SESSION", "LUVUS_ENV", "LUVUS_PANE_ID"):
        env.pop(key, None)
    env["LUVUS_HOME"] = str(home)
    command = [binary, "--session", args.session]

    def owner(method, params):
        response = subprocess.run(
            command + ["uhp", "proxy"], env=env,
            input=json.dumps({"id": "owner", "method": method, "params": params}) + "\n",
            text=True, capture_output=True, timeout=10, check=True,
        )
        value = json.loads(response.stdout)
        assert "error" not in value, value
        return value["result"]

    def exchange(endpoint, request):
        with socket.create_connection((endpoint["host"], endpoint["port"]), timeout=5) as stream:
            stream.sendall(json.dumps(request).encode() + b"\n")
            with stream.makefile("rb") as reader:
                return json.loads(reader.readline(1024 * 1024 + 1))

    server = gateway = None
    try:
        server = subprocess.Popen(command + ["server"], env=env, stdin=subprocess.DEVNULL,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        # The explicit home/session is used by every owner request, including
        # the readiness probe; no inherited socket can select production.
        def server_ready():
            if server.poll() is not None:
                raise RuntimeError("isolated server exited during startup")
            try:
                return bool(owner("uhp.capabilities", {}))
            except subprocess.CalledProcessError:
                return False
        wait_for(server_ready)
        pane = owner("pane.split", {})["pane"]
        log, ready = home / "input.bin", home / "ready"
        child = [sys.executable, str(pathlib.Path(__file__).resolve()), "--child", str(log), str(ready)]
        owner("pane.run", {"pane": pane, "command": "exec " + shlex.join(child)})
        wait_for(ready.exists)
        owner("pane.report_session", {"pane": pane, "agent": "codex", "session_id": "access-keys-fixture"})
        print(json.dumps({"binary": binary, "home": str(home), "session": args.session, "pane": pane}))
        for control in (False, True):
            gateway = subprocess.Popen(command + ["uhp", "access"] + (["--control"] if control else []),
                                       env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                       stderr=subprocess.DEVNULL, text=True)
            with selectors.DefaultSelector() as selector:
                selector.register(gateway.stdout, selectors.EVENT_READ)
                assert selector.select(10), "Access descriptor timeout"
                descriptor = json.loads(gateway.stdout.readline())
            endpoint = descriptor["endpoint"]
            pair = {"type": "pair", "code": descriptor["pairing"]["code"]}
            paired = exchange(endpoint, pair)
            token = paired["token"]
            replay = exchange(endpoint, pair)
            assert replay["error"]["code"] == "forbidden"
            before = log.read_bytes()
            for method, params, auth in [
                ("agent.keys", {"target": pane, "keys": ["enter"]}, "wrong"),
                ("agent.send", {"target": pane, "text": "never"}, token),
                ("pane.close", {"pane": pane}, token),
                ("agent.keys.", {"target": pane, "keys": ["enter"]}, token),
            ]:
                denied = exchange(endpoint, {"id": "denied", "method": method, "params": params, "auth": auth})
                assert denied["error"]["code"] == "forbidden", denied
            request = {"id": "keys", "method": "agent.keys", "params": {
                "target": pane, "keys": ["ctrl+c", "ctrl+z", "esc", "[", "Z", "y", "🙂"]}, "auth": token}
            response = exchange(endpoint, request)
            allowed = control and args.expect == "allowed"
            if allowed:
                assert response["result"]["type"] == "ok", response
                assert response["result"]["pane"] == pane, response
                expected = before + b"\x03\x1a\x1b[Zy" + "🙂".encode()
                wait_for(lambda: log.read_bytes() == expected)
                invalid = exchange(endpoint, {"id": "invalid", "method": "agent.keys", "params": {
                    "target": pane, "keys": ["enter", "not-a-key"]}, "auth": token})
                assert invalid["error"]["code"] == "invalid_request", invalid
            else:
                assert response["error"]["code"] == "forbidden", response
                expected = before
            # A quiet raw child makes partial admission observable as bytes.
            time.sleep(0.1)
            assert log.read_bytes() == expected
            print(json.dumps({"mode": "control" if control else "read_only", "response": response,
                              "input_hex": log.read_bytes().hex(), "rejections_admitted_bytes": 0}))
            stop(gateway)
            gateway = None
    finally:
        stop(gateway)
        stop(server)


if __name__ == "__main__":
    if len(sys.argv) == 4 and sys.argv[1] == "--child":
        import tty
        tty.setraw(sys.stdin.fileno())
        with open(sys.argv[2], "wb", buffering=0) as output:
            pathlib.Path(sys.argv[3]).touch()
            while data := os.read(sys.stdin.fileno(), 4096):
                output.write(data)
    else:
        main()
