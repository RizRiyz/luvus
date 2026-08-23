#!/usr/bin/env python3
"""Inject real terminal-backend v1 transport and lifecycle failures."""

import argparse
import json
import pathlib
import socket
import time

from consumer import request, validate_unix_endpoint
from live_support import ROOT, isolated_server


FRAME_LIMIT = 1024 * 1024


def raw_exchange(socket_path, payload, shutdown_write=False):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
        stream.settimeout(5)
        stream.connect(str(socket_path))
        try:
            stream.sendall(payload)
        except BrokenPipeError:
            # An oversized request may be rejected while the client is still
            # filling the local socket buffer. The bounded error response is
            # still authoritative and must remain readable below.
            pass
        if shutdown_write:
            stream.shutdown(socket.SHUT_WR)
        response = bytearray()
        while len(response) <= FRAME_LIMIT:
            chunk = stream.recv(65536)
            if not chunk:
                break
            response.extend(chunk)
        if len(response) > FRAME_LIMIT:
            raise RuntimeError("failure response exceeded the protocol frame limit")
        return bytes(response)


def subscribe(socket_path):
    stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    stream.settimeout(5)
    stream.connect(str(socket_path))
    stream.sendall(
        b'{"id":"events","method":"terminal.backend.events.subscribe","params":{}}\n'
    )
    reader = stream.makefile("rb")
    response = json.loads(reader.readline())
    return stream, reader, response


def locator(result):
    return {key: result[key] for key in ("server_generation", "terminal_id", "pane_id")}


def wait_for_label(socket_path, terminal_id, expected):
    deadline = time.monotonic() + 3
    while time.monotonic() < deadline:
        inventory = request(
            socket_path,
            {"id": "inventory-after-lost-response", "method": "terminal.backend.inventory", "params": {}},
        )
        terminal = next(
            (
                item
                for item in inventory["result"]["terminals"]
                if item["terminal_id"] == terminal_id
            ),
            None,
        )
        if terminal is not None and terminal.get("label") == expected:
            return
        time.sleep(0.025)
    raise RuntimeError("mutation with a deliberately lost response was not observable")


def main():
    if not hasattr(socket, "AF_UNIX"):
        raise SystemExit("failure conformance currently requires Unix local sockets")
    parser = argparse.ArgumentParser()
    parser.add_argument("--luvus", default=str(ROOT / "target" / "debug" / "luvus"))
    args = parser.parse_args()

    with isolated_server(args.luvus, "terminal-backend-failure-") as server:
        socket_path = server.socket_path
        evidence = validate_unix_endpoint(socket_path)

        missing_lf = raw_exchange(
            socket_path,
            b'{"id":"missing-lf","method":"terminal.backend.inventory","params":{}}',
            shutdown_write=True,
        )
        assert missing_lf == b"", "missing LF must close without dispatch"

        duplicate = raw_exchange(
            socket_path,
            b'{"id":"first","id":"second","method":"terminal.backend.inventory","params":{}}\n',
        )
        duplicate_response = json.loads(duplicate)
        assert duplicate_response["error"]["code"] == "invalid_request"

        oversized = raw_exchange(socket_path, b" " * FRAME_LIMIT + b"x\n")
        oversized_response = json.loads(oversized)
        assert oversized_response["error"]["code"] == "frame_too_large"
        assert oversized_response["id"] == "0"

        first = b'{"id":"first-frame","method":"terminal.backend.inventory","params":{}}\n'
        second = b'{"id":"second-frame","method":"terminal.backend.inventory","params":{}}\n'
        two_frames = raw_exchange(socket_path, first + second, shutdown_write=True)
        frames = two_frames.splitlines()
        assert len(frames) == 1 and json.loads(frames[0])["id"] == "first-frame"

        capability = request(
            socket_path,
            {
                "id": "capability",
                "method": "terminal.backend.capabilities",
                "params": {
                    "protocol": {
                        "name": "luvus-terminal-backend",
                        "major": 1,
                        "minor": 0,
                    }
                },
            },
        )
        generation = capability["result"]["server_generation"]
        created = request(
            socket_path,
            {
                "id": "create",
                "method": "terminal.backend.create",
                "params": {
                    "cwd": str(ROOT),
                    "command": ["/bin/sh", "-c", "cat"],
                    "label": "failure-conformance",
                    "placement": {"kind": "workspace"},
                    "focus": False,
                },
            },
        )
        runtime = locator(created["result"])

        stale_server = request(
            socket_path,
            {
                "id": "stale-server",
                "method": "terminal.backend.set_title",
                "params": dict(runtime, server_generation="0" * 32, title="must-not-apply"),
            },
        )
        assert stale_server["error"]["code"] == "stale_server"
        assert stale_server["error"]["dispatch"] == "rejected"

        stale_route = request(
            socket_path,
            {
                "id": "stale-route",
                "method": "terminal.backend.set_title",
                "params": dict(runtime, pane_id=str(int(runtime["pane_id"]) + 1000), title="must-not-apply"),
            },
        )
        assert stale_route["error"]["code"] == "stale_route"
        assert stale_route["error"]["dispatch"] == "rejected"

        capture = request(
            socket_path,
            {
                "id": "capture-before-wait",
                "method": "terminal.backend.capture",
                "params": dict(runtime, mode="visible", lines=24, ansi=False),
            },
        )
        revision = capture["result"]["content_revision"]
        timeout = request(
            socket_path,
            {
                "id": "wait-timeout",
                "method": "terminal.backend.wait_change",
                "params": dict(runtime, after_revision=revision, timeout_ms=50),
            },
        )
        assert timeout["error"]["code"] == "timeout"

        lost_request = {
            "id": "lost-response",
            "method": "terminal.backend.set_title",
            "params": dict(runtime, title="possibly-executed"),
        }
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
            stream.connect(str(socket_path))
            stream.sendall(json.dumps(lost_request, separators=(",", ":")).encode() + b"\n")
        wait_for_label(socket_path, runtime["terminal_id"], "possibly-executed")

        event_streams = []
        try:
            for _ in range(64):
                stream, reader, response = subscribe(socket_path)
                assert response["result"]["type"] == "subscription_started"
                event_streams.append((stream, reader))
            rejected_stream, rejected_reader, rejected = subscribe(socket_path)
            try:
                assert rejected["error"]["code"] == "unavailable"
            finally:
                rejected_reader.close()
                rejected_stream.close()
        finally:
            for stream, reader in event_streams:
                reader.close()
                stream.close()

        wait_stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        wait_stream.settimeout(0.2)
        wait_stream.connect(str(socket_path))
        wait_stream.sendall(
            json.dumps(
                {
                    "id": "cancelled-wait",
                    "method": "terminal.backend.wait_change",
                    "params": dict(runtime, after_revision=revision, timeout_ms=5000),
                },
                separators=(",", ":"),
            ).encode()
            + b"\n"
        )
        try:
            wait_stream.recv(1)
            raise AssertionError("wait_change answered before its terminal changed")
        except socket.timeout:
            pass
        closed = request(
            socket_path,
            {"id": "close", "method": "terminal.backend.close", "params": runtime},
        )
        assert closed["result"]["dispatch"] == "executed"
        wait_stream.settimeout(3)
        cancelled = bytearray()
        while not cancelled.endswith(b"\n"):
            chunk = wait_stream.recv(65536)
            if not chunk:
                raise RuntimeError("terminal close dropped its parked waiter response")
            cancelled.extend(chunk)
        wait_stream.close()
        assert json.loads(cancelled)["error"]["code"] == "terminal_gone"

        server.restart(previous_evidence=evidence)
        try:
            request(
                socket_path,
                {"id": "replaced-endpoint", "method": "terminal.backend.inventory", "params": {}},
                endpoint_evidence=evidence,
            )
            raise AssertionError("endpoint replacement was accepted")
        except RuntimeError as error:
            assert "endpoint was replaced" in str(error)

        restarted = request(
            socket_path,
            {
                "id": "capability-after-restart",
                "method": "terminal.backend.capabilities",
                "params": {
                    "protocol": {
                        "name": "luvus-terminal-backend",
                        "major": 1,
                        "minor": 0,
                    }
                },
            },
        )
        assert restarted["result"]["server_generation"] != generation
        old_runtime = request(
            socket_path,
            {"id": "old-runtime", "method": "terminal.backend.validate", "params": runtime},
        )
        assert old_runtime["error"]["code"] == "stale_server"

    print(
        "terminal-backend failure conformance passed: framing, identity, "
        "lost response, capacity, cancellation, and restart"
    )


if __name__ == "__main__":
    main()
