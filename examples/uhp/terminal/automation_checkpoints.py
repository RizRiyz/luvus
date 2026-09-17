#!/usr/bin/env python3
"""Opt-in Unix checkpoint/CLI/restart test using a fake worker, never a provider."""

import argparse
import concurrent.futures
import json
import pathlib
import statistics
import subprocess
import sys
import time

from consumer import request
from live_support import ROOT, isolated_server


def call(server, method, params=None):
    return request(server.socket_path, {
        "id": method, "method": method, "params": params or {},
    })


def install_fixture_worker(server):
    """Restrict worker resolution before admitting any runnable definition."""
    server.stop()
    fixtures = server.state / "fixture-bin"
    fixtures.mkdir()
    marker = server.state / "fixture-launches.jsonl"
    executable = fixtures / "codex"
    executable.write_text(
        f"#!{pathlib.Path(sys.executable).resolve()}\n"
        "import json, os, pathlib, sys\n"
        f"marker = pathlib.Path({str(marker)!r})\n"
        "record = {'argv': sys.argv[1:], 'task_id': os.environ.get('LUVUS_TASK_ID')}\n"
        "with marker.open('a') as stream:\n"
        "    stream.write(json.dumps(record) + '\\n')\n"
        "assert sys.argv[1:-1] == ['exec', '--sandbox', 'read-only', '-c', 'approval_policy=never']\n"
        "assert len(sys.argv) == 7 and record['task_id']\n"
        "assert 'Checkpoint fixture only.' in sys.argv[-1]\n"
        "print('checkpoint fixture completed')\n"
    )
    executable.chmod(0o700)
    # No user shell startup/PATH or real provider executable participates.
    server.environment["PATH"] = f"{fixtures}:/usr/bin:/bin"
    server.environment["SHELL"] = "/bin/sh"
    for key in ("ENV", "BASH_ENV", "ZDOTDIR"):
        server.environment.pop(key, None)
    (server.state / "config.json").write_text(json.dumps({"shell": "/bin/sh"}))
    server.start()
    return marker


def wait_for_completed_run(server, identity, run_id):
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        runs = call(server, "automation.history", {"id": identity})["result"]["runs"]
        run = next(item for item in runs if item["id"] == run_id)
        assert run["status"] not in ("failed", "cancelled", "skipped", "review"), run
        if run["status"] == "succeeded":
            return run
        time.sleep(0.05)
    raise AssertionError(f"fixture worker failed to complete: {run}")


def main():
    import fcntl

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--luvus", default=str(ROOT / "target/debug/luvus"))
    args = parser.parse_args()
    with isolated_server(pathlib.Path(args.luvus).resolve(strict=True), "automation-checkpoints-") as server:
        marker = install_fixture_worker(server)
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
        assert "result" in call(server, "automation.update", {
            **{key: value for key, value in params.items() if key != "idempotency_key"},
            "id": identity,
            "task": {**params["task"], "prompt": "Checkpoint fixture only."},
        })
        assert "result" in call(server, "automation.enable", {"id": identity})
        run_params = {"id": identity, "idempotency_key": "fixture-run-once"}
        admitted = call(server, "automation.run", run_params)
        assert "result" in admitted, admitted
        run_id = admitted["result"]["run"]["id"]
        assert call(server, "automation.run", run_params)["result"]["run"]["id"] == run_id
        completed = wait_for_completed_run(server, identity, run_id)
        task_id = completed["task_id"]
        assert call(server, "task.get", {"id": task_id})["result"]["task"]["status"] == "done"
        assert len(call(server, "task.list")["result"]["tasks"]) == 1
        launches = [json.loads(line) for line in marker.read_text().splitlines()]
        assert len(launches) == 1 and launches[0]["task_id"] == task_id, launches

        server.stop()
        server.start()
        retry = call(server, "automation.create", params)
        assert retry["result"]["automation"]["id"] == identity, retry
        assert len(call(server, "automation.list")["result"]["automations"]) == 1
        assert call(server, "automation.run", run_params)["result"]["run"]["id"] == run_id
        restored = wait_for_completed_run(server, identity, run_id)
        assert restored["task_id"] == task_id
        assert len(call(server, "automation.history")["result"]["runs"]) == 1
        assert len(call(server, "task.list")["result"]["tasks"]) == 1
        assert len(marker.read_text().splitlines()) == 1, "retry/restart launched the fixture twice"
        print(json.dumps({"checkpoint_ack_order": "passed", "cli": "passed",
                          "restart_idempotency": "passed", "external_agents_launched": 0,
                          "fixture_workers_launched": 1, "worker_completion": "passed",
                          "ping_samples": len(samples),
                          "blocked_worker_ping_median_ms": statistics.median(samples),
                          "blocked_worker_ping_max_ms": max(samples)}, indent=2))


if __name__ == "__main__":
    main()
