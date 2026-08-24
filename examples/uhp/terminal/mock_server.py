#!/usr/bin/env python3
"""One-frame fixture-driven Unix mock for UHP terminal adapter tests."""

import argparse
import json
import os
import pathlib
import socket

from consumer import REQUEST_ID, valid_request

ROOT = pathlib.Path(__file__).resolve().parents[3]
RESPONSES = ROOT / "protocol" / "uhp" / "v1" / "terminal" / "fixtures" / "valid" / "responses.jsonl"
MAX_FRAME = 1024 * 1024


def load_results():
    values = [json.loads(line) for line in RESPONSES.read_text().splitlines()]
    results = {}
    for value in values:
        if "result" in value:
            results.setdefault(value["result"].get("type"), value["result"])
    return results


def response_for(request, results):
    request_id = request.get("id", "0")
    method = request.get("method")
    result_type = {
        "uhp.capabilities": "uhp_capabilities",
        "terminal.backend.inventory": "terminal_backend_inventory",
        "terminal.backend.snapshot": "terminal_backend_snapshot",
        "terminal.backend.validate": "terminal_backend_validation",
        "terminal.backend.processes": "terminal_backend_processes",
        "terminal.backend.wait_change": "terminal_backend_change",
        "terminal.backend.wait_output": "terminal_backend_output",
    }.get(method)
    if result_type in results:
        result = dict(results[result_type])
        return {"id": request_id, "result": result}
    return {"id": request_id, "error": {"code": "unsupported_capability", "message": "mock method is not configured"}}


def invalid_request(request, message):
    request_id = request.get("id") if isinstance(request, dict) else None
    if not isinstance(request_id, str) or REQUEST_ID.fullmatch(request_id) is None:
        request_id = "0"
    return {"id": request_id, "error": {"code": "invalid_request", "message": message}}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--socket", required=True)
    parser.add_argument("--requests", type=int, default=2)
    args = parser.parse_args()
    path = pathlib.Path(args.socket)
    if not path.is_absolute() or path.exists():
        parser.error("--socket must be an unused absolute path")
    path.parent.mkdir(parents=True, exist_ok=True)
    results = load_results()
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
            listener.bind(str(path))
            path.chmod(0o600)
            listener.listen()
            for _ in range(args.requests):
                connection, _ = listener.accept()
                with connection:
                    frame = bytearray()
                    while not frame.endswith(b"\n") and len(frame) <= MAX_FRAME:
                        chunk = connection.recv(min(65536, MAX_FRAME + 1 - len(frame)))
                        if not chunk:
                            break
                        frame.extend(chunk)
                    if not frame.endswith(b"\n") or len(frame) > MAX_FRAME:
                        continue
                    try:
                        request = json.loads(frame[:-1])
                    except (UnicodeDecodeError, json.JSONDecodeError):
                        response = invalid_request(None, "bad json")
                    else:
                        response = (
                            response_for(request, results)
                            if valid_request(request)
                            else invalid_request(request, "invalid request")
                        )
                    connection.sendall(json.dumps(response, separators=(",", ":")).encode() + b"\n")
    finally:
        try:
            path.unlink()
        except FileNotFoundError:
            pass


if __name__ == "__main__":
    main()
