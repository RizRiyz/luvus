#!/usr/bin/env python3
"""Dependency-free validator for the public Luvus runtime API v1 package."""

import json
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
PACKAGE = ROOT / "protocol" / "runtime" / "v1"
PANE = re.compile(r"^[1-9][0-9]{0,9}$")
SOURCE = re.compile(r"^[A-Za-z][A-Za-z0-9._:/-]{0,63}$")
AGENT = re.compile(r"^[a-z][a-z0-9_-]{0,31}$")
STATES = {"idle", "working", "blocked", "done"}
RESULT_TYPES = {
    "runtime_capabilities",
    "session_snapshot",
    "pane_processes",
    "agent_explanation",
    "agent_report",
    "agent_release",
    "agent_wait",
    "subscription_started",
}
FIELDS = {
    "runtime.capabilities": set(),
    "session.snapshot": set(),
    "pane.processes": {"pane"},
    "agent.explain": {"target", "pane"},
    "agent.report": {
        "pane", "source", "agent", "status", "message", "session_id",
        "sequence", "ttl_s",
    },
    "agent.release": {"pane", "source"},
    "agent.wait": {"pane", "status", "timeout_s"},
    "events.subscribe": set(),
}


def unique_object(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise ValueError(f"duplicate object key: {key}")
        value[key] = item
    return value


def parse_unique(line):
    return json.loads(line, object_pairs_hook=unique_object)


def integer(value):
    return type(value) is int


def pane(value):
    return isinstance(value, str) and PANE.fullmatch(value) is not None


def bounded_string(value, maximum, allow_empty=True):
    return isinstance(value, str) and (allow_empty or bool(value)) and len(value) <= maximum


def valid_request(value):
    if not isinstance(value, dict) or set(value) != {"id", "method", "params"}:
        return False
    if not bounded_string(value["id"], 128, allow_empty=False):
        return False
    method = value["method"]
    params = value["params"]
    if method not in FIELDS or not isinstance(params, dict) or not set(params) <= FIELDS[method]:
        return False
    if method in {"runtime.capabilities", "session.snapshot", "events.subscribe"}:
        return not params
    if method == "pane.processes":
        return set(params) == {"pane"} and pane(params["pane"])
    if method == "agent.explain":
        if len(params) != 1:
            return False
        if "pane" in params:
            return pane(params["pane"])
        return bounded_string(params.get("target"), 128, allow_empty=False)
    if not pane(params.get("pane")):
        return False
    if method == "agent.report":
        if not {"pane", "source", "agent", "status"} <= set(params):
            return False
        if not isinstance(params["source"], str) or SOURCE.fullmatch(params["source"]) is None:
            return False
        if not isinstance(params["agent"], str) or AGENT.fullmatch(params["agent"]) is None:
            return False
        if params["status"] not in STATES:
            return False
        if "message" in params and not bounded_string(params["message"], 4096):
            return False
        if "session_id" in params and not bounded_string(params["session_id"], 512):
            return False
        if "sequence" in params and (not integer(params["sequence"]) or params["sequence"] < 0):
            return False
        if "ttl_s" in params and (not integer(params["ttl_s"]) or not 1 <= params["ttl_s"] <= 86400):
            return False
        return True
    if method == "agent.release":
        return (
            set(params) == {"pane", "source"}
            and isinstance(params["source"], str)
            and SOURCE.fullmatch(params["source"]) is not None
        )
    if method == "agent.wait":
        if not {"pane", "status"} <= set(params) or params["status"] not in STATES:
            return False
        timeout = params.get("timeout_s", 0)
        return type(timeout) in {int, float} and 0 <= timeout <= 3600
    return False


def valid_response(value):
    if not isinstance(value, dict) or not bounded_string(value.get("id"), 128, allow_empty=False):
        return False
    if set(value) == {"id", "result"}:
        result = value["result"]
        return isinstance(result, dict) and result.get("type") in RESULT_TYPES
    if set(value) == {"id", "error"}:
        error = value["error"]
        return (
            isinstance(error, dict)
            and bounded_string(error.get("code"), 128, allow_empty=False)
            and bounded_string(error.get("message"), 512)
        )
    return False


def valid_event(value):
    return (
        isinstance(value, dict)
        and set(value) == {"event", "sequence", "data"}
        and bounded_string(value["event"], 128, allow_empty=False)
        and integer(value["sequence"])
        and value["sequence"] >= 1
        and isinstance(value["data"], dict)
    )


def main():
    manifest = json.loads((PACKAGE / "fixtures" / "manifest.json").read_text())
    assert manifest["protocol"] == {"name": "luvus-runtime", "major": 1, "minor": 0}
    checked = 0
    for entry in manifest["files"]:
        lines = (PACKAGE / "fixtures" / entry["path"]).read_text().splitlines()
        assert len(lines) == entry["count"], entry["path"]
        validator = {"request": valid_request, "response": valid_response, "event": valid_event}[entry["kind"]]
        for line in lines:
            try:
                valid = validator(parse_unique(line))
            except (json.JSONDecodeError, ValueError, TypeError):
                valid = False
            assert valid == (entry["expect"] == "valid"), line
            checked += 1
    for path in (PACKAGE / "schema").rglob("*.json"):
        json.loads(path.read_text())
    print(f"validated {checked} runtime API fixtures and all JSON schema documents")
    return 0


if __name__ == "__main__":
    sys.exit(main())
