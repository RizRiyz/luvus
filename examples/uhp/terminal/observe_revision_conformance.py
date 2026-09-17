#!/usr/bin/env python3
"""Check final replacement frames through isolated named-session Access streams."""

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
    """Terminate and reap a fixture-owned process with a bounded fallback."""
    if process is not None and process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


def main():
    """Test observe/control transport; the deterministic race proof is in Rust."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--home", required=True)
    parser.add_argument("--session", default="luvus-pr-test")
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
        """Route one owner request through the explicitly selected namespace."""
        process = subprocess.run(command + ["uhp", "proxy"], env=env, capture_output=True,
                                 text=True, timeout=10, check=True,
                                 input=json.dumps({"id": "owner", "method": method, "params": params}) + "\n")
        response = json.loads(process.stdout)
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
        print(json.dumps({"binary": binary, "home": str(home), "session": args.session}))
        for control in (False, True):
            pane = owner("pane.split", {})["pane"]
            trigger = home / f"trigger-{control}"
            child = [sys.executable, str(pathlib.Path(__file__).resolve()), "--child", str(trigger)]
            owner("pane.run", {"pane": pane, "command": "exec " + shlex.join(child)})
            inventory = owner("terminal.backend.inventory", {})
            terminal = next(t for t in inventory["terminals"] if t["pane_id"] == pane)
            params = {key: terminal[key] for key in ("terminal_id", "pane_id")}
            params["server_generation"] = inventory["server_generation"]
            params.update(mode="recent_unwrapped", lines=200, ansi=True)
            gateway = subprocess.Popen(command + ["uhp", "access"] + (["--control"] if control else []),
                                       env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                       stderr=subprocess.DEVNULL, text=True)
            with selectors.DefaultSelector() as selector:
                selector.register(gateway.stdout, selectors.EVENT_READ)
                assert selector.select(10), "Access descriptor timeout"
                descriptor = json.loads(gateway.stdout.readline())
            address = (descriptor["endpoint"]["host"], descriptor["endpoint"]["port"])
            with socket.create_connection(address, timeout=5) as pair:
                pair.sendall(json.dumps({"type": "pair", "code": descriptor["pairing"]["code"]}).encode() + b"\n")
                with pair.makefile("rb") as reader:
                    token = json.loads(reader.readline())["token"]
            with socket.create_connection(address, timeout=5) as stream:
                method = "terminal.backend.control" if control else "terminal.backend.observe"
                stream.sendall(json.dumps({"id": "observe", "method": method, "params": params, "auth": token}).encode() + b"\n")
                with stream.makefile("rb") as reader:
                    ack = json.loads(reader.readline(1024 * 1024 + 1))
                    assert ack["result"]["type"] == "terminal_backend_stream", ack
                    # Trigger a final update before consuming the initial frame.
                    # Timing does not guarantee the baseline race: Rust's writer
                    # barrier test is the authoritative red/green evidence.
                    trigger.touch()
                    time.sleep(0.05)
                    revisions = []
                    deadline = time.monotonic() + 10
                    while time.monotonic() < deadline:
                        frame = json.loads(reader.readline(1024 * 1024 + 1))
                        assert frame["event"] == "terminal.frame", frame
                        revisions.append(frame["data"]["content_revision"])
                        if "FINAL_QUIET_MARKER" in frame["data"]["text"]:
                            print(json.dumps({"method": method, "revisions": revisions,
                                              "final_frame": frame, "timing_race_proof": False}))
                            break
                    else:
                        raise AssertionError("final quiet marker was not delivered")
            stop(gateway)
            gateway = None
    finally:
        stop(gateway)
        stop(server)


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--child":
        print("OBSERVE_READY", flush=True)
        while not pathlib.Path(sys.argv[2]).exists():
            time.sleep(0.01)
        print("FINAL_QUIET_MARKER", flush=True)
        while True:
            time.sleep(60)
    else:
        main()
