#!/usr/bin/env python3
"""Verify snapshot aliases through read-only Access in a fresh named session."""

import argparse
import json
import os
import pathlib
import selectors
import socket
import subprocess
import time


def stop(process):
    """Stop and reap a harness-owned process, including on assertion failure."""
    if process is not None and process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


def main():
    """Compare missing-field baseline and alias/null projection after the fix."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--home", required=True)
    parser.add_argument("--session", default="luvus-pr-test")
    parser.add_argument("--expect", choices=["missing", "present"], required=True)
    args = parser.parse_args()
    binary = str(pathlib.Path(args.binary).resolve(strict=True))
    home = pathlib.Path(args.home).resolve(strict=True)
    if any(home.iterdir()):
        raise ValueError("--home must be fresh and empty")
    env = os.environ.copy()
    for key in ("LUVUS_SOCKET_PATH", "LUVUS_SESSION", "LUVUS_ENV", "LUVUS_PANE_ID"):
        env.pop(key, None)
    env["LUVUS_HOME"] = str(home)
    command = [binary, "--session", args.session]

    def owner(method, params):
        """Use only the explicitly selected isolated owner namespace."""
        completed = subprocess.run(command + ["uhp", "proxy"], env=env, timeout=10,
                                   text=True, capture_output=True, check=True,
                                   input=json.dumps({"id": "owner", "method": method, "params": params}) + "\n")
        response = json.loads(completed.stdout)
        assert "error" not in response, response
        return response["result"]

    server = gateway = None
    try:
        server = subprocess.Popen(command + ["server"], env=env, stdin=subprocess.DEVNULL,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        deadline = time.monotonic() + 10
        while True:
            assert server.poll() is None, "isolated server exited"
            try:
                owner("uhp.capabilities", {})
                break
            except subprocess.CalledProcessError:
                assert time.monotonic() < deadline, "server startup timeout"
                time.sleep(0.025)
        pane = owner("pane.split", {})["pane"]
        owner("tab.new", {})
        gateway = subprocess.Popen(command + ["uhp", "access"], env=env, stdin=subprocess.DEVNULL,
                                   stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        with selectors.DefaultSelector() as selector:
            selector.register(gateway.stdout, selectors.EVENT_READ)
            assert selector.select(10), "Access descriptor timeout"
            descriptor = json.loads(gateway.stdout.readline())
        address = (descriptor["endpoint"]["host"], descriptor["endpoint"]["port"])

        def exchange(request):
            """Read one bounded response from the paired loopback gateway."""
            with socket.create_connection(address, timeout=5) as stream:
                stream.sendall(json.dumps(request).encode() + b"\n")
                with stream.makefile("rb") as reader:
                    return json.loads(reader.readline(1024 * 1024 + 1))

        token = exchange({"type": "pair", "code": descriptor["pairing"]["code"]})["token"]
        print(json.dumps({"binary": binary, "home": str(home), "session": args.session}))
        for alias in ("phone-fixture", "renamed-fixture", None):
            owner("agent.name", {"pane": pane, **({"name": alias} if alias else {"clear": True})})
            response = exchange({"id": "snapshot", "method": "session.snapshot", "params": {}, "auth": token})
            row = next(row for w in response["result"]["workspaces"] for t in w["tabs"]
                       for row in t["panes"] if row["pane_id"] == pane)
            assert not row["focused"]
            if args.expect == "present":
                assert "agent_name" in row and row["agent_name"] == alias, row
            else:
                assert "agent_name" not in row, row
            print(json.dumps({"assigned_alias": alias, "snapshot_row": row}))
    finally:
        stop(gateway)
        stop(server)


if __name__ == "__main__":
    main()
