#!/usr/bin/env python3
"""Dependency-free validator for the public Luvus UHP 1.0 package."""

import json
import pathlib
import re
import sys
import unicodedata

ROOT = pathlib.Path(__file__).resolve().parents[2]
PACKAGE = ROOT / "protocol" / "uhp" / "v1"
PANE = re.compile(r"^[1-9][0-9]{0,9}$")
SOURCE = re.compile(r"^[A-Za-z][A-Za-z0-9._:/-]{0,63}$")
AGENT = re.compile(r"^[a-z][a-z0-9_-]{0,31}$")
REQUEST_ID = re.compile(r"^[A-Za-z0-9._:-]{1,128}$")
SESSION_NAME = re.compile(r"^[A-Za-z0-9._-]{1,64}$")
INTEGRATION_AGENT = re.compile(r"^[a-z0-9._-]{1,64}$")
EMPTY_HOST_METHODS = {
    "host.capabilities", "host.info", "host.doctor", "host.update.check",
    "session.list", "skill.status", "integration.status",
}
SESSION_TARGET_METHODS = {
    "session.status", "session.start", "session.stop", "session.restart",
}
STATES = {"idle", "working", "blocked", "done"}
KEY_NAMES = {
    "enter", "return", "cr", "esc", "escape", "tab", "space", "backspace", "bs",
    "delete", "del", "up", "down", "right", "left", "home", "end", "pageup",
    "pgup", "pagedown", "pgdn",
}
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
    "agent.wait": {"pane", "status", "statuses", "timeout_s"},
    "agent.keys": {"target", "keys"},
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


def session_name(value):
    return (
        isinstance(value, str)
        and value not in {".", ".."}
        and SESSION_NAME.fullmatch(value) is not None
    )


def valid_agent_wait_params(params):
    if not isinstance(params, dict) or not set(params) <= FIELDS["agent.wait"]:
        return False
    if not pane(params.get("pane")):
        return False
    has_status = "status" in params
    has_statuses = "statuses" in params
    if has_status == has_statuses:
        return False
    if has_status and params["status"] not in STATES:
        return False
    if has_statuses:
        statuses = params["statuses"]
        if not isinstance(statuses, list) or not 1 <= len(statuses) <= len(STATES):
            return False
        if any(not isinstance(status, str) for status in statuses):
            return False
        if len(set(statuses)) != len(statuses) or not set(statuses) <= STATES:
            return False
    timeout = params.get("timeout_s", 0)
    return type(timeout) in {int, float} and 0 <= timeout <= 3600


def valid_automation_trigger(value):
    if not isinstance(value, dict) or not isinstance(value.get("kind"), str):
        return False
    kind = value["kind"]
    if kind == "once":
        return set(value) == {"kind", "at_utc"} and integer(value["at_utc"]) and value["at_utc"] >= 1
    if kind == "interval":
        return (
            set(value) == {"kind", "every_seconds", "anchor_utc"}
            and integer(value["every_seconds"])
            and value["every_seconds"] >= 60
            and integer(value["anchor_utc"])
            and value["anchor_utc"] >= 1
        )
    if kind == "daily":
        return (
            set(value) == {"kind", "timezone", "second_of_day"}
            and bounded_string(value["timezone"], 128, allow_empty=False)
            and integer(value["second_of_day"])
            and 0 <= value["second_of_day"] <= 86399
        )
    if kind == "weekly":
        weekdays = value.get("weekdays")
        return (
            set(value) == {"kind", "timezone", "weekdays", "second_of_day"}
            and bounded_string(value["timezone"], 128, allow_empty=False)
            and isinstance(weekdays, list)
            and 1 <= len(weekdays) <= 7
            and len(set(weekdays)) == len(weekdays)
            and all(integer(day) and 1 <= day <= 7 for day in weekdays)
            and integer(value["second_of_day"])
            and 0 <= value["second_of_day"] <= 86399
        )
    return False


def valid_automation_task(value):
    allowed = {
        "title",
        "prompt",
        "agent_id",
        "workspace_id",
        "mode",
        "access",
        "paths",
        "gate",
    }
    required = {"title", "prompt", "agent_id", "workspace_id"}
    if not isinstance(value, dict) or not required <= set(value) or not set(value) <= allowed:
        return False
    if not bounded_string(value["title"], 256, allow_empty=False):
        return False
    if not bounded_string(value["prompt"], 32768, allow_empty=False):
        return False
    if not bounded_string(value["agent_id"], 64, allow_empty=False):
        return False
    if not bounded_string(value["workspace_id"], 128, allow_empty=False):
        return False
    if "mode" in value and value["mode"] not in {"worktree", "workspace"}:
        return False
    if "access" in value and value["access"] not in {
        "read_only",
        "workspace",
        "full_access",
    }:
        return False
    if "gate" in value and value["gate"] is not None and not bounded_string(value["gate"], 4096):
        return False
    paths = value.get("paths", [])
    return (
        isinstance(paths, list)
        and len(paths) <= 64
        and len(set(paths)) == len(paths)
        and all(bounded_string(path, 1024, allow_empty=False) for path in paths)
    )


def valid_automation_policy(value):
    if not isinstance(value, dict) or not set(value) <= {
        "misfire", "overlap", "misfire_grace_seconds"
    }:
        return False
    if "misfire" in value and value["misfire"] not in {"skip", "run_latest"}:
        return False
    if "overlap" in value and value["overlap"] not in {"skip", "queue_one"}:
        return False
    grace = value.get("misfire_grace_seconds", 0)
    return integer(grace) and 0 <= grace <= 31536000


def valid_automation_target(value):
    if not isinstance(value, dict):
        return False
    if value.get("kind") == "new_worker":
        return set(value) == {"kind"}
    if value.get("kind") != "active_agent":
        return False
    if not {"kind", "pane_id", "terminal_id"} <= set(value):
        return False
    if not set(value) <= {"kind", "pane_id", "terminal_id", "if_busy"}:
        return False
    pane_id = value["pane_id"]
    if isinstance(pane_id, str):
        valid_pane = PANE.fullmatch(pane_id) is not None and int(pane_id) <= 4294967295
    else:
        valid_pane = integer(pane_id) and 1 <= pane_id <= 4294967295
    return (
        valid_pane
        and isinstance(value["terminal_id"], str)
        and re.fullmatch(r"[0-9a-f]{32}", value["terminal_id"]) is not None
        and value.get("if_busy", "wait") in {"wait", "skip"}
    )


def valid_automation_definition(params, *, update):
    allowed = {
        "id", "name", "enabled", "trigger", "target", "task", "policy", "idempotency_key"
    }
    required = {"name", "trigger", "task"} | ({"id"} if update else set())
    if not required <= set(params) or not set(params) <= allowed:
        return False
    if (update and "idempotency_key" in params) or (not update and "id" in params):
        return False
    if "id" in params and not bounded_string(params["id"], 128, allow_empty=False):
        return False
    if not bounded_string(params["name"], 128, allow_empty=False):
        return False
    if "enabled" in params and type(params["enabled"]) is not bool:
        return False
    if not valid_automation_trigger(params["trigger"]):
        return False
    if "target" in params and not valid_automation_target(params["target"]):
        return False
    if not valid_automation_task(params["task"]):
        return False
    if "policy" in params and not valid_automation_policy(params["policy"]):
        return False
    return "idempotency_key" not in params or bounded_string(
        params["idempotency_key"], 128, allow_empty=False
    )


def agent_key(value):
    if not isinstance(value, str):
        return False
    lower = value.lower()
    if lower in KEY_NAMES:
        return True
    if re.fullmatch(r"(?:ctrl\+|c-)[a-z]", lower) is not None:
        return True
    return len(value) == 1 and unicodedata.category(value) not in {"Cc", "Cs"}


def valid_agent_keys_params(params):
    return (
        set(params) == {"target", "keys"}
        and bounded_string(params["target"], 128, allow_empty=False)
        and isinstance(params["keys"], list)
        and bool(params["keys"])
        and all(agent_key(key) for key in params["keys"])
    )


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
        return valid_agent_wait_params(params)
    if method == "agent.keys":
        return valid_agent_keys_params(params)
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
                and (
                    not result["matched"]
                    or (pane(result["pane"]) and result["status"] in STATES)
                )
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
    if "auth" in value:
        auth = value["auth"]
        if not bounded_string(auth, 256, allow_empty=False) or not all(
            "!" <= char <= "~" for char in auth
        ):
            return False
    if value["method"] == "uhp.token.create":
        scopes = value["params"].get("scopes")
        ttl = value["params"].get("ttl_s", 3600)
        return isinstance(scopes, list) and bool(scopes) and 1 <= ttl <= 86400
    if value["method"] == "events.subscribe":
        after = value["params"].get("after_sequence", 0)
        return integer(after) and after >= 0
    if value["method"] == "agent.wait":
        return valid_agent_wait_params(value["params"])
    if value["method"] == "agent.keys":
        return valid_agent_keys_params(value["params"])
    if value["method"] in EMPTY_HOST_METHODS:
        return not value["params"]
    if value["method"] in SESSION_TARGET_METHODS:
        params = value["params"]
        return set(params) == {"name"} and session_name(params["name"])
    if value["method"] in {"host.update.install", "skill.enable", "skill.disable"}:
        return value["params"] == {"confirm": True}
    if value["method"] in {"integration.install", "integration.uninstall"}:
        params = value["params"]
        return (
            set(params) == {"agent", "confirm"}
            and params["confirm"] is True
            and isinstance(params["agent"], str)
            and INTEGRATION_AGENT.fullmatch(params["agent"]) is not None
        )
    if value["method"] == "session.delete":
        params = value["params"]
        return (
            set(params) == {"name", "confirm"}
            and params["confirm"] is True
            and session_name(params["name"])
        )
    if value["method"] == "task.start":
        params = value["params"]
        if not set(params) <= {"id", "branch", "agent", "mode", "workspace_id"}:
            return False
        if not bounded_string(params.get("id"), 128, allow_empty=False):
            return False
        if "branch" in params and not bounded_string(params["branch"], 255, allow_empty=False):
            return False
        if "agent" in params and not bounded_string(params["agent"], 4096, allow_empty=False):
            return False
        if "workspace_id" in params and not bounded_string(
            params["workspace_id"], 128, allow_empty=False
        ):
            return False
        mode = params.get("mode")
        if mode is not None and (not isinstance(mode, str) or mode not in {"worktree", "workspace"}):
            return False
        if mode == "workspace" and "branch" in params:
            return False
    if value["method"] == "task.heartbeat":
        params = value["params"]
        context = params.get("context")
        return (
            set(params) == {"id", "context"}
            and bounded_string(params["id"], 128, allow_empty=False)
            and isinstance(context, (int, float))
            and not isinstance(context, bool)
            and 0 <= context <= 1
        )
    if value["method"] in {"automation.list", "automation.health"}:
        return not value["params"]
    if value["method"] in {
        "automation.get", "automation.enable", "automation.disable", "automation.delete"
    }:
        params = value["params"]
        return (
            set(params) == {"id"}
            and bounded_string(params["id"], 128, allow_empty=False)
        )
    if value["method"] == "automation.run":
        params = value["params"]
        return (
            set(params) <= {"id", "idempotency_key"}
            and "id" in params
            and bounded_string(params["id"], 128, allow_empty=False)
            and (
                "idempotency_key" not in params
                or bounded_string(params["idempotency_key"], 128, allow_empty=False)
            )
        )
    if value["method"] == "automation.history":
        params = value["params"]
        return (
            set(params) <= {"id", "limit"}
            and (
                "id" not in params
                or bounded_string(params["id"], 128, allow_empty=False)
            )
            and (
                "limit" not in params
                or integer(params["limit"])
                and 1 <= params["limit"] <= 200
            )
        )
    if value["method"] == "automation.preview":
        params = value["params"]
        return (
            set(params) <= {"trigger", "from_utc"}
            and "trigger" in params
            and valid_automation_trigger(params["trigger"])
            and (
                "from_utc" not in params
                or integer(params["from_utc"])
                and params["from_utc"] >= 0
            )
        )
    if value["method"] == "automation.create":
        return valid_automation_definition(value["params"], update=False)
    if value["method"] == "automation.update":
        return valid_automation_definition(value["params"], update=True)
    return True


def valid_global_response(value):
    if not (
        isinstance(value, dict)
        and isinstance(value.get("id"), str)
        and REQUEST_ID.fullmatch(value["id"]) is not None
        and ((set(value) == {"id", "result"}) != (set(value) == {"id", "error"}))
    ):
        return False
    if set(value) == {"id", "result"} and not isinstance(value["result"], dict):
        return False
    if set(value) == {"id", "error"} and not isinstance(value["error"], dict):
        return False
    result = value.get("result")
    if isinstance(result, dict) and result.get("type") == "agent_wait":
        return valid_response(value)
    return True


def main():
    assert not agent_key("\ud800"), "Unicode surrogates are not valid key scalars"
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
