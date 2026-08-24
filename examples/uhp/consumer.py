#!/usr/bin/env python3
"""Dependency-free validator for the public Luvus UHP 1.0 package."""

import json
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
PACKAGE = ROOT / "protocol" / "uhp" / "v1"
PANE = re.compile(r"^[1-9][0-9]{0,9}$")
SOURCE = re.compile(r"^[A-Za-z][A-Za-z0-9._:/-]{0,63}$")
AGENT = re.compile(r"^[a-z][a-z0-9_-]{0,31}$")
REQUEST_ID = re.compile(r"^[A-Za-z0-9._:-]{1,128}$")
STATES = {"idle", "working", "blocked", "done"}
RESULT_TYPES = {
    "uhp_capabilities",
    "session_snapshot",
    "pane_processes",
    "agent_explanation",
    "agent_report",
    "agent_release",
    "agent_start",
    "agent_prompt",
    "agent_wait",
    "subscription_started",
}
RESULT_FIELDS = {
    "uhp_capabilities": {
        "protocol", "session", "event_sequence", "methods", "agent_authorities",
        "agent_states", "limits",
    },
    "session_snapshot": {
        "protocol", "session", "server_generation", "event_sequence", "workspaces",
    },
    "pane_processes": {
        "pane", "terminal_id", "root_process", "scan", "executables", "arguments_exposed",
    },
    "agent_explanation": {"pane", "available"},
    "agent_report": {"pane", "agent", "status", "source", "sequence", "ttl_s"},
    "agent_release": {"pane"},
    "agent_start": {"name", "kind", "pane", "ready", "status"},
    "agent_prompt": {
        "pane", "submitted", "matched", "status", "baseline_revision",
        "content_revision", "evidence",
    },
    "agent_wait": {"matched", "pane", "status"},
    "subscription_started": {"sequence", "queue_capacity", "loss_behavior"},
}
FIELDS = {
    "uhp.capabilities": set(),
    "session.snapshot": set(),
    "pane.processes": {"pane"},
    "agent.explain": {"target", "pane"},
    "agent.report": {
        "pane", "source", "agent", "status", "message", "session_id",
        "sequence", "ttl_s",
    },
    "agent.release": {"pane", "source"},
    "agent.start": {"name", "kind", "pane", "anchor", "direction", "args", "timeout_s"},
    "agent.prompt": {"target", "text", "wait", "until", "timeout_s"},
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
    if not isinstance(value["id"], str) or REQUEST_ID.fullmatch(value["id"]) is None:
        return False
    method = value["method"]
    params = value["params"]
    if method not in FIELDS or not isinstance(params, dict) or not set(params) <= FIELDS[method]:
        return False
    if method in {"uhp.capabilities", "session.snapshot", "events.subscribe"}:
        return not params
    if method == "pane.processes":
        return set(params) == {"pane"} and pane(params["pane"])
    if method == "agent.explain":
        if len(params) != 1:
            return False
        if "pane" in params:
            return pane(params["pane"])
        return bounded_string(params.get("target"), 128, allow_empty=False)
    if method == "agent.report":
        if not pane(params.get("pane")):
            return False
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
            pane(params.get("pane"))
            and set(params) == {"pane", "source"}
            and isinstance(params["source"], str)
            and SOURCE.fullmatch(params["source"]) is not None
        )
    if method == "agent.start":
        if not {"name", "kind"} <= set(params) or {"pane", "anchor"} <= set(params):
            return False
        if (
            not isinstance(params["name"], str)
            or AGENT.fullmatch(params["name"]) is None
            or not isinstance(params["kind"], str)
            or AGENT.fullmatch(params["kind"]) is None
        ):
            return False
        if "pane" in params and not pane(params["pane"]):
            return False
        if "anchor" in params and not pane(params["anchor"]):
            return False
        if params.get("direction", "right") not in {"right", "down"}:
            return False
        args = params.get("args", [])
        if not isinstance(args, list) or len(args) > 64:
            return False
        if not all(bounded_string(arg, 4096) and not any(c in arg for c in "\0\r\n") for arg in args):
            return False
        timeout = params.get("timeout_s", 30)
        return type(timeout) in {int, float} and 0 <= timeout <= 3600
    if method == "agent.prompt":
        if not {"target", "text"} <= set(params):
            return False
        if not bounded_string(params["target"], 128, allow_empty=False):
            return False
        if not bounded_string(params["text"], 262144, allow_empty=False):
            return False
        if "wait" in params and type(params["wait"]) is not bool:
            return False
        wait = params.get("wait", False)
        if not wait and ({"until", "timeout_s"} & set(params)):
            return False
        until = params.get("until", ["idle", "done", "blocked"])
        if not isinstance(until, list) or not 1 <= len(until) <= 4:
            return False
        if len(set(until)) != len(until) or not set(until) <= STATES:
            return False
        timeout = params.get("timeout_s", 300)
        return type(timeout) in {int, float} and 0 <= timeout <= 3600
    if method == "agent.wait":
        if not pane(params.get("pane")) or not {"pane", "status"} <= set(params) or params["status"] not in STATES:
            return False
        timeout = params.get("timeout_s", 0)
        return type(timeout) in {int, float} and 0 <= timeout <= 3600
    return False


def valid_response(value):
    if not isinstance(value, dict) or not isinstance(value.get("id"), str) or REQUEST_ID.fullmatch(value["id"]) is None:
        return False
    if set(value) == {"id", "result"}:
        result = value["result"]
        if not isinstance(result, dict) or result.get("type") not in RESULT_TYPES:
            return False
        required = RESULT_FIELDS[result["type"]]
        if not required <= set(result):
            return False
        kind = result["type"]
        if kind in {"uhp_capabilities", "session_snapshot"}:
            protocol = result["protocol"]
            if protocol != {"name": "luvus-uhp", "major": 1, "minor": 0}:
                return False
            if not isinstance(result["session"], str) or not integer(result["event_sequence"]):
                return False
            if kind == "uhp_capabilities":
                return (
                    isinstance(result["methods"], list)
                    and isinstance(result["agent_authorities"], list)
                    and isinstance(result["agent_states"], list)
                    and set(result["agent_states"]) <= STATES
                    and isinstance(result["limits"], dict)
                )
            return (
                bounded_string(result["server_generation"], 512, allow_empty=False)
                and isinstance(result["workspaces"], list)
            )
        if kind == "pane_processes":
            return (
                pane(result["pane"])
                and (result["terminal_id"] is None or isinstance(result["terminal_id"], str))
                and (result["root_process"] is None or isinstance(result["root_process"], dict))
                and result["scan"] in {"observed", "unavailable"}
                and isinstance(result["executables"], list)
                and all(isinstance(item, str) for item in result["executables"])
                and result["arguments_exposed"] is False
            )
        if kind == "agent_explanation":
            if not pane(result["pane"]) or type(result["available"]) is not bool:
                return False
            if not result["available"]:
                return True
            return (
                {"agent", "status", "identity", "state_evidence", "authority", "session"}
                <= set(result)
                and isinstance(result["agent"], str)
                and result["status"] in STATES
                and isinstance(result["identity"], dict)
                and isinstance(result["state_evidence"], dict)
                and (result["authority"] is None or isinstance(result["authority"], dict))
                and (result["session"] is None or isinstance(result["session"], dict))
            )
        if kind == "agent_report":
            return (
                pane(result["pane"])
                and isinstance(result["agent"], str)
                and result["status"] in STATES
                and isinstance(result["source"], str)
                and integer(result["sequence"])
                and result["sequence"] >= 0
                and integer(result["ttl_s"])
                and 1 <= result["ttl_s"] <= 86400
            )
        if kind == "agent_release":
            return pane(result["pane"])
        if kind == "agent_start":
            return (
                isinstance(result["name"], str)
                and isinstance(result["kind"], str)
                and pane(result["pane"])
                and type(result["ready"]) is bool
                and (result["status"] is None or result["status"] in STATES)
            )
        if kind == "agent_prompt":
            return (
                pane(result["pane"])
                and type(result["submitted"]) is bool
                and type(result["matched"]) is bool
                and (result["status"] is None or result["status"] in STATES)
                and integer(result["baseline_revision"])
                and result["baseline_revision"] >= 0
                and integer(result["content_revision"])
                and result["content_revision"] >= 0
                and result["evidence"] in {
                    "queued", "state_transition", "output_settled", "timeout", "pane_closed",
                }
            )
        if kind == "agent_wait":
            return (
                type(result["matched"]) is bool
                and (result["pane"] is None or pane(result["pane"]))
                and (result["status"] is None or result["status"] in STATES)
            )
        return (
            integer(result["sequence"])
            and result["sequence"] >= 0
            and integer(result["queue_capacity"])
            and result["queue_capacity"] >= 1
            and result["loss_behavior"] == "resync_required_then_close"
        )
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


def valid_global_request(value, methods):
    if not isinstance(value, dict) or not set(value) <= {"id", "method", "params", "auth"}:
        return False
    if not {"id", "method", "params"} <= set(value):
        return False
    if not isinstance(value["id"], str) or REQUEST_ID.fullmatch(value["id"]) is None:
        return False
    if value["method"] not in methods or not isinstance(value["params"], dict):
        return False
    if "auth" in value and not bounded_string(value["auth"], 256, allow_empty=False):
        return False
    if value["method"] == "uhp.token.create":
        scopes = value["params"].get("scopes")
        ttl = value["params"].get("ttl_s", 3600)
        return isinstance(scopes, list) and bool(scopes) and 1 <= ttl <= 86400
    if value["method"] == "events.subscribe":
        after = value["params"].get("after_sequence", 0)
        return integer(after) and after >= 0
    return True


def valid_global_response(value):
    return (
        isinstance(value, dict)
        and isinstance(value.get("id"), str)
        and REQUEST_ID.fullmatch(value["id"]) is not None
        and ((set(value) == {"id", "result"}) != (set(value) == {"id", "error"}))
    )


def main():
    manifest = json.loads((PACKAGE / "fixtures" / "manifest.json").read_text())
    assert manifest["protocol"] == {"name": "luvus-uhp", "major": 1, "minor": 0}
    request_schema = json.loads((PACKAGE / "schema" / "request.schema.json").read_text())
    methods = set(request_schema["properties"]["method"]["enum"])
    checked = 0
    for entry in manifest["files"]:
        lines = (PACKAGE / "fixtures" / entry["path"]).read_text().splitlines()
        assert len(lines) == entry["count"], entry["path"]
        validator = {
            "request": lambda value: valid_global_request(value, methods),
            "response": valid_global_response,
            "event": valid_event,
        }[entry["kind"]]
        for line in lines:
            try:
                valid = validator(parse_unique(line))
            except (json.JSONDecodeError, ValueError, TypeError):
                valid = False
            assert valid == (entry["expect"] == "valid"), line
            checked += 1
    for path in (PACKAGE / "schema").rglob("*.json"):
        json.loads(path.read_text())
    print(f"validated {checked} UHP fixtures and all JSON schema documents")
    return 0


if __name__ == "__main__":
    sys.exit(main())
