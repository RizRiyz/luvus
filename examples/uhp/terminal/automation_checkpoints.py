#!/usr/bin/env python3
"""Opt-in Unix automation checkpoint/CLI/restart test; never launches an agent."""

import argparse
import concurrent.futures
import json
import pathlib
import statistics
import subprocess
import time

from consumer import request
from live_support import ROOT, isolated_server


def call(server, method, params=None):
    return request(server.socket_path, {
        "id": method, "method": method, "params": params or {},
    })


def main():
    import fcntl

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--luvus", default=str(ROOT / "target/debug/luvus"))
    args = parser.parse_args()
    with isolated_server(pathlib.Path(args.luvus).resolve(strict=True), "automation-checkpoints-") as server:
        workspaces = call(server, "workspace.list")["result"]["workspaces"]
        params = {
            "name": "checkpoint regression", "enabled": False,
            "trigger": {"kind": "once", "at_utc": int(time.time()) + 3600},
            "task": {
                "title": "checkpoint regression", "prompt": "Do not execute this disabled fixture.",
                "agent_id": "codex", "workspace_id": workspaces[0]["workspace_id"],
                "mode": "workspace", "access": "read_only",
            },
            "idempotency_key": "checkpoint-regression",
        }
        # Hold the existing shared worker in a bounded config-lock acquisition.
        # Automation acknowledgement must wait, while unrelated API stays live.
        samples = []
        with (server.state / "config.lock").open("a+b") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            assert "result" in call(server, "config.patch", {"patch": {"agents_this_workspace": True}})
            with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
                future = pool.submit(call, server, "automation.create", params)
                time.sleep(0.05)
                assert not future.done(), "automation acknowledged before its checkpoint"
                for _ in range(50):
                    start = time.perf_counter()
                    assert "result" in call(server, "ping")
                    samples.append((time.perf_counter() - start) * 1000)
                fcntl.flock(lock, fcntl.LOCK_UN)
                created = future.result(timeout=10)
        assert "result" in created, created
        identity = created["result"]["automation"]["id"]
        ledger = json.loads((server.state / "automations.json").read_text())
        assert any(item["id"] == identity for item in ledger["automations"])
        assert not ledger["runs"], "disabled fixture must not launch anything"
        retry = call(server, "automation.create", params)
        assert retry["result"]["automation"]["id"] == identity
        cli = subprocess.run([str(server.binary), "automation", "list"],
                             env=server.environment, cwd=ROOT, capture_output=True,
                             text=True, check=True, timeout=10)
        assert identity in cli.stdout
        server.stop()
        server.start()
        retry = call(server, "automation.create", params)
        assert retry["result"]["automation"]["id"] == identity, retry
        assert len(call(server, "automation.list")["result"]["automations"]) == 1
        assert not call(server, "automation.history")["result"]["runs"]
        print(json.dumps({"checkpoint_ack_order": "passed", "cli": "passed",
                          "restart_idempotency": "passed", "agents_launched": 0,
                          "ping_samples": len(samples),
                          "blocked_worker_ping_median_ms": statistics.median(samples),
                          "blocked_worker_ping_max_ms": max(samples)}, indent=2))


if __name__ == "__main__":
    main()
