#!/usr/bin/env python3
"""Run terminal-backend v1 against an isolated development Luvus server."""

import argparse
import os
import pathlib
import json
import socket
import subprocess
import tempfile
import time

from consumer import reconcile_snapshot, request

ROOT = pathlib.Path(__file__).resolve().parents[3]


def locator(result):
    return {key: result[key] for key in ("server_generation", "terminal_id", "pane_id")}


def subscribe(socket_path, method="terminal.backend.events.subscribe"):
    stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    stream.settimeout(5)
    stream.connect(str(socket_path))
    reader = stream.makefile("rb")
    stream.sendall(json.dumps({
        "id": "events",
        "method": method,
        "params": {},
    }, separators=(",", ":")).encode() + b"\n")
    acknowledgement = json.loads(reader.readline())
    assert acknowledgement["result"]["type"] == "subscription_started"
    assert acknowledgement["result"]["loss_behavior"] == "resync_required_then_close"
    return stream, reader, acknowledgement["result"]


def wait_event(reader, expected, terminal_id=None):
    while True:
        event = json.loads(reader.readline())
        if event.get("event") == "terminal.resync_required":
            raise RuntimeError("terminal event subscriber overflowed during conformance")
        if event.get("event") != expected:
            continue
        if terminal_id is None or event.get("data", {}).get("terminal_id") == terminal_id:
            return event


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--luvus", default=str(ROOT / "target" / "debug" / "luvus"))
    args = parser.parse_args()
    binary = pathlib.Path(args.luvus).resolve(strict=True)
    target = ROOT / "target"
    target.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="terminal-backend-live-", dir=target) as state:
        socket_path = pathlib.Path(state) / "luvus.sock"
        environment = os.environ.copy()
        environment["LUVUS_HOME"] = state
        for key in (
            "LUVUS_SOCKET_PATH",
            "LUVUS_SESSION",
            "LUVUS_ENV",
            "LUVUS_PANE_ID",
        ):
            environment.pop(key, None)
        server = subprocess.Popen(
            [str(binary), "server"],
            cwd=ROOT,
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            deadline = time.monotonic() + 10
            while not socket_path.exists():
                if server.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("isolated Luvus server did not start")
                time.sleep(0.025)

            capability = request(socket_path, {"id":"cap","method":"uhp.capabilities","params":{}})
            assert capability["result"]["protocol"] == {"name":"luvus-uhp","major":1,"minor":0}
            cli_capability = json.loads(subprocess.run(
                [str(binary), "uhp", "capabilities"],
                cwd=ROOT,
                env=environment,
                check=True,
                capture_output=True,
                text=True,
            ).stdout)
            assert cli_capability["result"]["protocol"] == {
                "name": "luvus-uhp", "major": 1, "minor": 0
            }
            runtime_capability = request(socket_path, {
                "id": "runtime-capability",
                "method": "uhp.capabilities",
                "params": {},
            })
            assert runtime_capability["result"]["protocol"] == {
                "name": "luvus-uhp",
                "major": 1,
                "minor": 0,
            }
            proxied = json.loads(subprocess.run(
                [str(binary), "uhp", "proxy"],
                cwd=ROOT,
                env=environment,
                input='{"id":"proxy","method":"uhp.capabilities","params":{}}\n',
                check=True,
                capture_output=True,
                text=True,
            ).stdout)
            assert proxied["result"]["type"] == "uhp_capabilities"
            event_stream, event_reader, subscription = subscribe(socket_path)
            runtime_stream, runtime_reader, runtime_subscription = subscribe(
                socket_path, "events.subscribe"
            )
            created = request(socket_path, {"id":"create","method":"terminal.backend.create","params":{"cwd":str(ROOT),"command":["/bin/sh","-c","cat"],"label":"live-conformance","placement":{"kind":"workspace"},"focus":False}})
            assert created["result"]["dispatch"] == "executed"
            runtime = locator(created["result"])
            session = request(socket_path, {
                "id": "session",
                "method": "session.snapshot",
                "params": {},
            })
            assert session["result"]["type"] == "session_snapshot"
            assert any(
                pane.get("terminal_id") == runtime["terminal_id"]
                for workspace in session["result"]["workspaces"]
                for tab in workspace["tabs"]
                for pane in tab["panes"]
            )
            pane_processes = request(socket_path, {
                "id": "pane-processes",
                "method": "pane.processes",
                "params": {"pane": runtime["pane_id"]},
            })
            assert pane_processes["result"]["arguments_exposed"] is False
            report = request(socket_path, {
                "id": "agent-report",
                "method": "agent.report",
                "params": {
                    "pane": runtime["pane_id"],
                    "source": "live-conformance",
                    "agent": "future-agent",
                    "status": "working",
                    "sequence": 1,
                    "ttl_s": 30,
                },
            })
            assert report["result"]["source"] == "live-conformance"
            explanation = request(socket_path, {
                "id": "agent-explain",
                "method": "agent.explain",
                "params": {"target": runtime["pane_id"]},
            })
            assert explanation["result"]["agent"] == "future-agent"
            assert explanation["result"]["authority"]["source"] == "live-conformance"
            authority_event = wait_event(runtime_reader, "agent.authority_reported")
            assert runtime_subscription["sequence"] < authority_event["sequence"]
            assert authority_event["data"]["pane"] == runtime["pane_id"]
            waited_agent = request(socket_path, {
                "id": "agent-wait",
                "method": "agent.wait",
                "params": {
                    "pane": runtime["pane_id"],
                    "statuses": ["blocked", "working"],
                    "timeout_s": 1,
                },
            })
            assert waited_agent["result"]["matched"] is True
            assert waited_agent["result"]["status"] == "working"
            released = request(socket_path, {
                "id": "agent-release",
                "method": "agent.release",
                "params": {
                    "pane": runtime["pane_id"],
                    "source": "live-conformance",
                },
            })
            assert released["result"]["type"] == "agent_release"
            snapshot = request(socket_path, {"id":"snapshot","method":"terminal.backend.snapshot","params":{}})
            created_event = wait_event(event_reader, "terminal.created", runtime["terminal_id"])
            assert subscription["sequence"] < created_event["sequence"] <= snapshot["result"]["event_sequence"]
            replayed = reconcile_snapshot(snapshot["result"], [created_event])
            assert not replayed["resnapshot_required"]
            assert any(terminal["terminal_id"] == runtime["terminal_id"] for terminal in replayed["terminals"])
            validated = request(socket_path, {"id":"validate","method":"terminal.backend.validate","params":runtime})
            assert validated["result"]["state"] == "alive"
            processes = request(socket_path, {"id":"processes","method":"terminal.backend.processes","params":runtime})
            assert processes["result"]["type"] == "terminal_backend_processes"
            assert processes["result"]["arguments_exposed"] is False
            assert isinstance(processes["result"]["executables"], list)
            captured = request(socket_path, {"id":"capture-before","method":"terminal.backend.capture","params":dict(runtime, mode="visible", lines=24, ansi=False)})
            before_revision = captured["result"]["content_revision"]
            submitted = request(socket_path, {"id":"submit","method":"terminal.backend.submit_text","params":dict(runtime, text="LUVUS_LIVE_CONFORMANCE")})
            assert submitted["result"]["dispatch"] == "queued"
            waited = request(socket_path, {"id":"wait-output","method":"terminal.backend.wait_output","params":dict(runtime, after_revision=before_revision, match="LUVUS_LIVE_CONFORMANCE", timeout_ms=3000)})
            assert waited["result"]["content_revision"] > before_revision
            output_event = wait_event(event_reader, "terminal.output_ready", runtime["terminal_id"])
            replayed = reconcile_snapshot(replayed, [output_event])
            assert not replayed["resnapshot_required"]
            assert next(terminal for terminal in replayed["terminals"] if terminal["terminal_id"] == runtime["terminal_id"])["content_revision"] >= waited["result"]["content_revision"]
            captured = request(socket_path, {"id":"capture","method":"terminal.backend.capture","params":dict(runtime, mode="visible", lines=24, ansi=False)})
            assert "LUVUS_LIVE_CONFORMANCE" in captured["result"]["text"]
            closed = request(socket_path, {"id":"close","method":"terminal.backend.close","params":runtime})
            assert closed["result"]["dispatch"] == "executed"
            closed_event = wait_event(event_reader, "terminal.closed", runtime["terminal_id"])
            replayed = reconcile_snapshot(replayed, [closed_event])
            assert not replayed["resnapshot_required"]
            assert not any(terminal["terminal_id"] == runtime["terminal_id"] for terminal in replayed["terminals"])
            event_stream.close()
            runtime_stream.close()
            gone = request(socket_path, {"id":"gone","method":"terminal.backend.validate","params":runtime})
            assert gone["result"]["state"] == "gone"
            print("terminal-backend live conformance passed in isolated LUVUS_HOME")
        finally:
            server.terminate()
            try:
                server.wait(timeout=3)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait()


if __name__ == "__main__":
    main()
