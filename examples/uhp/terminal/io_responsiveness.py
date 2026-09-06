#!/usr/bin/env python3
"""Opt-in Unix settings-lock responsiveness check in an isolated server."""

import argparse
import json
import pathlib
import statistics
import subprocess
import time

from consumer import request
from live_support import ROOT, isolated_server


def summary(samples):
    ordered = sorted(samples)
    return {"samples": len(samples), "median_ms": statistics.median(samples),
            "p95_ms": ordered[(len(ordered) - 1) * 95 // 100], "max_ms": max(samples)}


def measure(server):
    api_samples = []
    for index in range(100):
        start = time.perf_counter()
        response = request(server.socket_path, {
            "id": f"probe-{index}", "method": "uhp.capabilities", "params": {},
        })
        api_samples.append((time.perf_counter() - start) * 1000)
        assert "result" in response, response
    cli_samples = []
    for _ in range(5):
        start = time.perf_counter()
        result = subprocess.run(
            [str(server.binary), "uhp", "capabilities"], env=server.environment,
            cwd=ROOT, capture_output=True, text=True, check=True, timeout=5,
        )
        cli_samples.append((time.perf_counter() - start) * 1000)
        assert "result" in json.loads(result.stdout)
    return {"uhp": summary(api_samples), "cli": summary(cli_samples)}


def main():
    import fcntl  # Unix-only fault injection, not a Windows validation claim.

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--luvus", default=str(ROOT / "target/debug/luvus"))
    args = parser.parse_args()
    binary = pathlib.Path(args.luvus).resolve(strict=True)
    with isolated_server(binary, "io-responsiveness-") as server:
        baseline = measure(server)
        with (server.state / "config.lock").open("a+b") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            start = time.perf_counter()
            response = request(server.socket_path, {
                "id": "settings", "method": "config.patch",
                "params": {"patch": {"agents_this_workspace": True}},
            })
            admission_ms = (time.perf_counter() - start) * 1000
            assert "result" in response, response
            time.sleep(0.1)
            blocked = measure(server)
            # Exercise the worker's lock timeout too. No throughput threshold:
            # report host-dependent timings rather than turn noise into CI red.
            time.sleep(1.1)
            fcntl.flock(lock, fcntl.LOCK_UN)
        response = request(server.socket_path, {
            "id": "reload", "method": "server.reload_config", "params": {},
        })
        assert response.get("result", {}).get("config", {}).get("agents_this_workspace") is True, response
        persisted = json.loads((server.state / "config.json").read_text())
        assert persisted["agents_this_workspace"] is True
        print(json.dumps({"binary": str(binary), "baseline": baseline,
                          "locked_storage": blocked, "settings_admission_ms": admission_ms,
                          "retry_persisted": True}, indent=2))


if __name__ == "__main__":
    main()
