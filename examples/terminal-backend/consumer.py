#!/usr/bin/env python3
"""Dependency-free Luvus terminal-backend v1 example and fixture checker."""

import argparse
import copy
import json
import os
import pathlib
import re
import socket
import stat
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
PACKAGE = ROOT / "protocol" / "terminal-backend" / "v1"
OPAQUE = re.compile(r"^[0-9a-f]{32}$")
KEYS = {"enter", "escape", "tab", "backtab", "up", "down", "left", "right", "home", "end", "backspace", "delete", "pageup", "pagedown", "ctrl-c", "ctrl-d", "ctrl-u", "ctrl-w", "space", *(f"digit-{n}" for n in range(10))}
METHOD_FIELDS = {
    "terminal.backend.capabilities": {"protocol"},
    "terminal.backend.inventory": set(),
    "terminal.backend.snapshot": set(),
    "terminal.backend.validate": {"server_generation", "terminal_id", "pane_id", "expected_root"},
    "terminal.backend.processes": {"server_generation", "terminal_id", "pane_id", "expected_root"},
    "terminal.backend.capture": {"server_generation", "terminal_id", "pane_id", "expected_root", "mode", "lines", "ansi"},
    "terminal.backend.type_literal": {"server_generation", "terminal_id", "pane_id", "expected_root", "text"},
    "terminal.backend.submit_text": {"server_generation", "terminal_id", "pane_id", "expected_root", "text"},
    "terminal.backend.send_key": {"server_generation", "terminal_id", "pane_id", "expected_root", "key"},
    "terminal.backend.set_title": {"server_generation", "terminal_id", "pane_id", "expected_root", "title"},
    "terminal.backend.notify": {"server_generation", "terminal_id", "pane_id", "expected_root", "title", "body"},
    "terminal.backend.create": {"cwd", "command", "label", "placement", "focus"},
    "terminal.backend.close": {"server_generation", "terminal_id", "pane_id", "expected_root"},
    "terminal.backend.events.subscribe": set(),
    "terminal.backend.wait_change": {"server_generation", "terminal_id", "pane_id", "expected_root", "after_revision", "timeout_ms"},
    "terminal.backend.wait_output": {"server_generation", "terminal_id", "pane_id", "expected_root", "after_revision", "match", "timeout_ms"},
}


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate object key: {key}")
        result[key] = value
    return result


def parse_unique(line):
    return json.loads(line, object_pairs_hook=unique_object)


def locator_ok(params):
    generation = params.get("server_generation")
    terminal_id = params.get("terminal_id")
    pane_id = params.get("pane_id")
    return (
        isinstance(generation, str)
        and OPAQUE.fullmatch(generation) is not None
        and isinstance(terminal_id, str)
        and OPAQUE.fullmatch(terminal_id) is not None
        and isinstance(pane_id, str)
        and re.fullmatch(r"[1-9][0-9]{0,9}", pane_id) is not None
    )


def valid_request(value):
    if not isinstance(value, dict) or set(value) != {"id", "method", "params"}:
        return False
    if not isinstance(value["id"], str) or not 1 <= len(value["id"].encode()) <= 128:
        return False
    method, params = value["method"], value["params"]
    if method not in METHOD_FIELDS or not isinstance(params, dict) or not set(params) <= METHOD_FIELDS[method]:
        return False
    if method in {"terminal.backend.inventory", "terminal.backend.snapshot", "terminal.backend.events.subscribe"}:
        return not params
    if method == "terminal.backend.capabilities":
        protocol = params.get("protocol")
        return isinstance(protocol, dict) and protocol == {"name": "luvus-terminal-backend", "major": 1, "minor": 0}
    if method == "terminal.backend.create":
        placement = params.get("placement", {})
        if not isinstance(params.get("cwd"), str) or not params["cwd"].startswith("/") or not isinstance(params.get("focus"), bool):
            return False
        if placement.get("kind") == "workspace":
            return set(placement) == {"kind"}
        return placement.get("kind") == "sibling" and set(placement) == {"kind", "of_terminal"} and locator_ok(placement["of_terminal"])
    if not locator_ok(params):
        return False
    if method in {"terminal.backend.wait_change", "terminal.backend.wait_output"}:
        if not isinstance(params.get("after_revision"), int) or params["after_revision"] < 0:
            return False
        if not isinstance(params.get("timeout_ms"), int) or not 1 <= params["timeout_ms"] <= 300000:
            return False
        return method == "terminal.backend.wait_change" or isinstance(params.get("match"), str) and 1 <= len(params["match"]) <= 4096
    if method == "terminal.backend.capture":
        return params.get("mode") in {"visible", "recent_unwrapped", "detection"} and isinstance(params.get("lines"), int) and 1 <= params["lines"] <= 300 and isinstance(params.get("ansi"), bool) and not (params["mode"] == "detection" and params["ansi"])
    if method == "terminal.backend.send_key":
        return params.get("key") in KEYS
    if method in {"terminal.backend.type_literal", "terminal.backend.submit_text"}:
        text = params.get("text")
        return isinstance(text, str) and 1 <= len(text.encode()) <= 262144
    return True


def valid_response(value):
    return isinstance(value, dict) and isinstance(value.get("id"), str) and ((isinstance(value.get("result"), dict) and "error" not in value) or (isinstance(value.get("error"), dict) and "result" not in value))


def valid_event(value):
    if not isinstance(value, dict) or set(value) != {"event", "sequence", "data"}:
        return False
    if not isinstance(value["sequence"], int) or value["sequence"] < 1 or not isinstance(value["data"], dict):
        return False
    if value["event"] == "terminal.resync_required":
        return value["data"] == {"reason": "subscriber_overflow"}
    if value["event"] not in {"terminal.created", "terminal.moved", "terminal.metadata_changed", "terminal.output_ready", "terminal.exited", "terminal.closed"}:
        return False
    data = value["data"]
    return set(data) == {"server_generation", "terminal_id", "pane_id", "content_revision", "workspace", "tab", "detail"} and locator_ok(data) and isinstance(data["content_revision"], int) and data["content_revision"] >= 0 and (data["workspace"] is None or isinstance(data["workspace"], int) and data["workspace"] >= 1) and (data["tab"] is None or isinstance(data["tab"], int) and data["tab"] >= 1) and isinstance(data["detail"], dict)


def reconcile_snapshot(snapshot, buffered_events):
    """Apply events after a snapshot fence and report when a fresh snapshot is required."""
    if not isinstance(snapshot, dict) or snapshot.get("type") != "terminal_backend_snapshot":
        raise ValueError("expected a terminal backend snapshot result")
    generation = snapshot.get("server_generation")
    fence = snapshot.get("event_sequence")
    terminals = snapshot.get("terminals")
    if OPAQUE.fullmatch(generation or "") is None or not isinstance(fence, int) or fence < 0 or not isinstance(terminals, list):
        raise ValueError("invalid terminal backend snapshot")

    if any(not isinstance(terminal, dict) or not isinstance(terminal.get("terminal_id"), str) for terminal in terminals):
        raise ValueError("invalid terminal inventory entry")
    reconciled = {terminal["terminal_id"]: copy.deepcopy(terminal) for terminal in terminals}
    if len(reconciled) != len(terminals):
        raise ValueError("duplicate terminal identity in snapshot")
    last_sequence = fence
    resnapshot_required = bool(snapshot.get("truncated") or snapshot.get("resnapshot_required"))
    for event in buffered_events:
        if not valid_event(event):
            raise ValueError("invalid terminal backend event")
        sequence = event["sequence"]
        if event["event"] == "terminal.resync_required":
            resnapshot_required = True
            last_sequence = max(last_sequence, sequence)
            break
        if sequence <= fence:
            continue
        if sequence <= last_sequence:
            raise ValueError("terminal backend events are not monotonic")
        last_sequence = sequence
        data = event["data"]
        if data["server_generation"] != generation:
            resnapshot_required = True
            break

        terminal_id = data["terminal_id"]
        terminal = reconciled.get(terminal_id)
        if event["event"] == "terminal.closed":
            reconciled.pop(terminal_id, None)
        elif event["event"] == "terminal.output_ready" and terminal is not None:
            if data["content_revision"] < terminal.get("content_revision", 0):
                resnapshot_required = True
                break
            terminal["content_revision"] = data["content_revision"]
        elif event["event"] == "terminal.metadata_changed" and terminal is not None and set(data["detail"]) == {"label"}:
            terminal["label"] = data["detail"].get("label") or None
        else:
            # Created, moved, exited, unknown metadata, and events for an
            # unknown terminal need authoritative inventory data.
            resnapshot_required = True
            break

    result = copy.deepcopy(snapshot)
    result["event_sequence"] = last_sequence
    result["terminals"] = list(reconciled.values())
    result["resnapshot_required"] = resnapshot_required
    return result


def check_reconciliation():
    generation = "1" * 32
    terminal_id = "2" * 32
    terminal = {
        "terminal_id": terminal_id,
        "pane_id": "7",
        "workspace": {"index": 1, "name": "work", "root": "/work"},
        "tab": {"index": 1, "name": "shell"},
        "cwd": "/work",
        "root_process": {"pid": 123, "start_marker": "example"},
        "content_revision": 2,
        "terminal_title": None,
        "label": None,
    }
    snapshot = {
        "type": "terminal_backend_snapshot",
        "server_generation": generation,
        "event_sequence": 12,
        "terminals": [terminal],
        "truncated": False,
    }

    def event(name, sequence, revision=2, detail=None):
        return {
            "event": name,
            "sequence": sequence,
            "data": {
                "server_generation": generation,
                "terminal_id": terminal_id,
                "pane_id": "7",
                "content_revision": revision,
                "workspace": 1,
                "tab": 1,
                "detail": detail or {},
            },
        }

    replayed = reconcile_snapshot(snapshot, [
        event("terminal.created", 12),
        event("terminal.output_ready", 13, revision=5),
        event("terminal.metadata_changed", 14, revision=5, detail={"label": "review"}),
    ])
    assert replayed["event_sequence"] == 14
    assert replayed["terminals"][0]["content_revision"] == 5
    assert replayed["terminals"][0]["label"] == "review"
    assert not replayed["resnapshot_required"]

    closed = reconcile_snapshot(replayed, [event("terminal.closed", 15, revision=5)])
    assert closed["terminals"] == [] and not closed["resnapshot_required"]
    overflow = reconcile_snapshot(snapshot, [{"event": "terminal.resync_required", "sequence": 16, "data": {"reason": "subscriber_overflow"}}])
    assert overflow["resnapshot_required"]
    assert reconcile_snapshot(overflow, [event("terminal.output_ready", 17, revision=6)])["resnapshot_required"]
    created = reconcile_snapshot(snapshot, [event("terminal.created", 16)])
    assert created["resnapshot_required"]


def check_fixtures():
    manifest = json.loads((PACKAGE / "fixtures" / "manifest.json").read_text())
    checked = 0
    for entry in manifest["files"]:
        lines = (PACKAGE / "fixtures" / entry["path"]).read_text().splitlines()
        assert len(lines) == entry["count"], entry["path"]
        for line in lines:
            try:
                value = parse_unique(line)
                if entry["kind"] == "request":
                    valid = valid_request(value)
                elif entry["kind"] == "response":
                    valid = valid_response(value)
                else:
                    valid = valid_event(value)
            except (json.JSONDecodeError, ValueError):
                valid = False
            assert valid == (entry["expect"] == "valid"), line
            checked += 1
    for path in (PACKAGE / "schema").rglob("*.json"):
        json.loads(path.read_text())
    endpoints = json.loads((PACKAGE / "fixtures" / "endpoint-validation.json").read_text())
    assert {item["expect"] for item in endpoints} == {"accept", "reject"}
    check_reconciliation()
    print(f"validated {checked} fixtures, all JSON schema documents, and snapshot replay")


def validate_unix_endpoint(sock_path):
    path = pathlib.Path(sock_path)
    if not path.is_absolute():
        raise RuntimeError("endpoint address is not absolute")
    chain = list(reversed(path.parents)) + [path]
    for component in chain:
        info = component.lstat()
        if stat.S_ISLNK(info.st_mode):
            raise RuntimeError("endpoint path contains a symlink")
    socket_info = path.lstat()
    if not stat.S_ISSOCK(socket_info.st_mode):
        raise RuntimeError("endpoint is not a Unix socket")
    uid = os.geteuid()
    if socket_info.st_uid != uid or stat.S_IMODE(socket_info.st_mode) != 0o600:
        raise RuntimeError("endpoint socket is not owner-only")
    owner_dir = path.parent.lstat()
    if owner_dir.st_uid != uid or stat.S_IMODE(owner_dir.st_mode) != 0o700:
        raise RuntimeError("endpoint directory is not owner-only")

    temporary_root = pathlib.Path("/private/tmp" if sys.platform == "darwin" else "/tmp")
    if temporary_root in path.parents:
        expected_dir = temporary_root / f"luvus-{uid}"
        root_info = temporary_root.lstat()
        if path.parent != expected_dir or root_info.st_uid != 0 or stat.S_IMODE(root_info.st_mode) != 0o1777:
            raise RuntimeError("unsafe temporary socket alias")
    else:
        for parent in path.parents:
            if parent == pathlib.Path("/"):
                continue
            info = parent.lstat()
            if info.st_mode & 0o022:
                raise RuntimeError("endpoint ancestor is group- or world-writable")
    if path.resolve(strict=True) != path:
        raise RuntimeError("endpoint canonical path changed")
    return socket_info.st_dev, socket_info.st_ino, socket_info.st_ctime_ns


def request(sock_path, request_value, endpoint_evidence=None):
    frame = json.dumps(request_value, separators=(",", ":")).encode() + b"\n"
    if len(frame) > 1024 * 1024:
        raise ValueError("request exceeds v1 frame limit")
    current_evidence = validate_unix_endpoint(sock_path)
    if endpoint_evidence is not None and current_evidence != endpoint_evidence:
        raise RuntimeError("endpoint was replaced")
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
        stream.connect(str(sock_path))
        stream.sendall(frame)
        response = bytearray()
        while not response.endswith(b"\n"):
            chunk = stream.recv(min(65536, 1024 * 1024 + 1 - len(response)))
            if not chunk:
                raise RuntimeError("response ended before LF")
            response.extend(chunk)
            if len(response) > 1024 * 1024:
                raise RuntimeError("response exceeds v1 frame limit")
    value = parse_unique(response[:-1].decode("utf-8"))
    if value.get("id") != request_value["id"]:
        raise RuntimeError("response id mismatch")
    return value


def inspect_endpoint(sock_path, session_name="configured"):
    evidence = validate_unix_endpoint(sock_path)
    capabilities = request(sock_path, {"id":"example-capabilities","method":"terminal.backend.capabilities","params":{"protocol":{"name":"luvus-terminal-backend","major":1,"minor":0}}}, evidence)
    if "error" in capabilities:
        raise RuntimeError(f"{session_name}: capability negotiation failed")
    inventory = request(sock_path, {"id":"example-inventory","method":"terminal.backend.inventory","params":{}}, evidence)
    inspected = {"session":session_name,"capabilities":capabilities,"inventory":inventory}
    print(json.dumps(inspected, indent=2))
    return inspected


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--fixtures", action="store_true")
    parser.add_argument("--socket")
    parser.add_argument("--discover", action="store_true")
    parser.add_argument("--luvus", default="luvus", help="luvus executable used only for discovery")
    args = parser.parse_args()
    if args.fixtures:
        check_fixtures()
    if args.socket:
        inspect_endpoint(args.socket)
    if args.discover:
        completed = subprocess.run([args.luvus, "session", "list", "--json"], check=True, capture_output=True, text=True)
        sessions = json.loads(completed.stdout)["sessions"]
        for session in sessions:
            if not session.get("running"):
                continue
            endpoint = session.get("endpoint", {})
            if endpoint.get("transport") != "unix_socket":
                continue
            inspect_endpoint(endpoint["address"], session["name"])
    if not args.fixtures and not args.socket and not args.discover:
        parser.error("choose --fixtures, --socket, or --discover")


if __name__ == "__main__":
    sys.exit(main())
