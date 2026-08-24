#!/usr/bin/env python3
"""Measure UHP terminal-method cost with isolated 1/10/50-pane servers."""

import argparse
import hashlib
import json
import math
import os
import pathlib
import platform
import re
import socket
import statistics
import subprocess
import sys
import time

from consumer import request
from live_support import ROOT, isolated_server


def locator(result):
    return {key: result[key] for key in ("server_generation", "terminal_id", "pane_id")}


def percentile(values, fraction):
    ordered = sorted(values)
    return ordered[max(0, math.ceil(len(ordered) * fraction) - 1)]


def latency_summary(values):
    if not values:
        return {"samples": 0, "median_ms": None, "p95_ms": None, "max_ms": None}
    return {
        "samples": len(values),
        "median_ms": round(statistics.median(values), 4),
        "p95_ms": round(percentile(values, 0.95), 4),
        "max_ms": round(max(values), 4),
    }


def timed_requests(socket_path, method, params, samples):
    latencies = []
    sizes = []
    response = None
    for index in range(5 + samples):
        started = time.perf_counter_ns()
        response = request(
            socket_path,
            {"id": f"bench-{method}-{index}", "method": method, "params": params},
        )
        elapsed_ms = (time.perf_counter_ns() - started) / 1_000_000
        if "error" in response:
            raise RuntimeError(f"{method} failed during benchmark: {response['error']}")
        if index >= 5:
            latencies.append(elapsed_ms)
            sizes.append(len(json.dumps(response, separators=(",", ":")).encode()) + 1)
    return response, {**latency_summary(latencies), "median_response_bytes": int(statistics.median(sizes))}


def parse_cpu_time(value):
    value = value.strip()
    days = 0
    if "-" in value:
        raw_days, value = value.split("-", 1)
        days = int(raw_days)
    parts = value.split(":")
    if len(parts) == 2:
        hours, minutes, seconds = 0, int(parts[0]), float(parts[1])
    elif len(parts) == 3:
        hours, minutes, seconds = int(parts[0]), int(parts[1]), float(parts[2])
    else:
        raise RuntimeError(f"unsupported process CPU time: {value}")
    return days * 86400 + hours * 3600 + minutes * 60 + seconds


def process_cpu_seconds(pid):
    if sys.platform.startswith("linux"):
        stat_line = pathlib.Path(f"/proc/{pid}/stat").read_text()
        fields = stat_line[stat_line.rfind(")") + 2 :].split()
        ticks = int(fields[11]) + int(fields[12])
        return ticks / os.sysconf("SC_CLK_TCK")
    output = subprocess.check_output(
        ["ps", "-o", "time=", "-p", str(pid)], text=True
    )
    return parse_cpu_time(output)


def process_rss_bytes(pid):
    if sys.platform.startswith("linux"):
        status = pathlib.Path(f"/proc/{pid}/status").read_text()
        match = re.search(r"^VmRSS:\s+(\d+)\s+kB$", status, re.MULTILINE)
        return int(match.group(1)) * 1024 if match else None
    output = subprocess.check_output(
        ["ps", "-o", "rss=", "-p", str(pid)], text=True
    ).strip()
    return int(output) * 1024 if output else None


def process_threads(pid):
    if sys.platform.startswith("linux"):
        return len(list(pathlib.Path(f"/proc/{pid}/task").iterdir()))
    output = subprocess.check_output(["ps", "-M", "-p", str(pid)], text=True)
    return max(0, len(output.splitlines()) - 1)


def process_descriptors(pid):
    if sys.platform.startswith("linux"):
        return len(list(pathlib.Path(f"/proc/{pid}/fd").iterdir()))
    try:
        output = subprocess.check_output(
            ["lsof", "-a", "-p", str(pid), "-Ff"],
            text=True,
            stderr=subprocess.DEVNULL,
        )
    except (FileNotFoundError, subprocess.CalledProcessError):
        return None
    return sum(line[1:].isdigit() for line in output.splitlines() if line.startswith("f"))


def macos_memory(pid):
    if sys.platform != "darwin":
        return {"physical_footprint_bytes": None, "peak_footprint_bytes": None, "live_heap_bytes": None}
    try:
        footprint = subprocess.check_output(
            ["footprint", "--noCategories", "-f", "bytes", "-p", str(pid)],
            text=True,
            stderr=subprocess.DEVNULL,
        )
        physical = re.search(r"phys_footprint:\s+(\d+) B", footprint)
        peak = re.search(r"phys_footprint_peak:\s+(\d+) B", footprint)
    except (FileNotFoundError, subprocess.CalledProcessError):
        physical = peak = None
    try:
        heap = subprocess.check_output(
            ["heap", str(pid)], text=True, stderr=subprocess.DEVNULL
        )
        live = re.search(r"All zones:\s+\d+ nodes \((\d+) bytes\)", heap)
    except (FileNotFoundError, subprocess.CalledProcessError):
        live = None
    return {
        "physical_footprint_bytes": int(physical.group(1)) if physical else None,
        "peak_footprint_bytes": int(peak.group(1)) if peak else None,
        "live_heap_bytes": int(live.group(1)) if live else None,
    }


def binary_digest(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def git_value(*args):
    return subprocess.check_output(["git", *args], cwd=ROOT, text=True).strip()


def benchmark_case(binary, pane_count, samples, idle_seconds):
    with isolated_server(binary, f"terminal-backend-bench-{pane_count}-") as server:
        initial = request(
            server.socket_path,
            {"id": "inventory-initial", "method": "terminal.backend.inventory", "params": {}},
        )
        initial_terminals = initial["result"]["terminals"]
        if len(initial_terminals) != 1:
            raise RuntimeError("fresh isolated server did not start with exactly one shell pane")
        generation = initial["result"]["server_generation"]
        runtimes = [
            {
                "server_generation": generation,
                "terminal_id": initial_terminals[0]["terminal_id"],
                "pane_id": initial_terminals[0]["pane_id"],
            }
        ]
        create_latencies = []
        for index in range(1, pane_count):
            started = time.perf_counter_ns()
            created = request(
                server.socket_path,
                {
                    "id": f"create-{index}",
                    "method": "terminal.backend.create",
                    "params": {
                        "cwd": str(ROOT),
                        "command": ["/bin/sh"],
                        "label": f"protocol-benchmark-{index + 1}",
                        "placement": {"kind": "workspace"},
                        "focus": False,
                    },
                },
            )
            if created.get("result", {}).get("dispatch") != "executed":
                raise RuntimeError(f"pane {index + 1} failed to start: {created}")
            create_latencies.append((time.perf_counter_ns() - started) / 1_000_000)
            runtimes.append(locator(created["result"]))

        inventory = request(
            server.socket_path,
            {"id": "inventory-count", "method": "terminal.backend.inventory", "params": {}},
        )
        if len(inventory["result"]["terminals"]) != pane_count:
            raise RuntimeError("inventory did not report the exact created pane count")

        first = runtimes[0]
        _, capabilities = timed_requests(
            server.socket_path,
            "uhp.capabilities",
            {},
            samples,
        )
        _, inventory_metrics = timed_requests(
            server.socket_path, "terminal.backend.inventory", {}, samples
        )
        _, capture = timed_requests(
            server.socket_path,
            "terminal.backend.capture",
            dict(first, mode="visible", lines=24, ansi=False),
            samples,
        )
        _, input_ack = timed_requests(
            server.socket_path,
            "terminal.backend.type_literal",
            dict(first, text="x"),
            samples,
        )

        time.sleep(1)
        cpu_before = process_cpu_seconds(server.process.pid)
        time.sleep(idle_seconds)
        cpu_after = process_cpu_seconds(server.process.pid)
        cpu_delta = max(0.0, cpu_after - cpu_before)
        resources = {
            "idle_window_seconds": idle_seconds,
            "idle_cpu_seconds": round(cpu_delta, 4),
            "idle_cpu_one_core_percent": round(cpu_delta / idle_seconds * 100, 4),
            "rss_bytes": process_rss_bytes(server.process.pid),
            "threads": process_threads(server.process.pid),
            "open_descriptors": process_descriptors(server.process.pid),
            **macos_memory(server.process.pid),
        }

        for index, runtime in enumerate(runtimes):
            closed = request(
                server.socket_path,
                {"id": f"close-{index}", "method": "terminal.backend.close", "params": runtime},
            )
            if closed.get("result", {}).get("dispatch") != "executed":
                raise RuntimeError(f"pane {index + 1} failed to close")

        return {
            "panes": pane_count,
            "additional_panes_created": pane_count - 1,
            "create": latency_summary(create_latencies),
            "capabilities": capabilities,
            "inventory": inventory_metrics,
            "capture_visible_24": capture,
            "input_queue_ack": input_ack,
            "resources": resources,
        }


def print_summary(report):
    print("\nTerminal backend protocol benchmark")
    print("panes  inventory p50/p95   capture p50/p95   input p50/p95   RSS MiB   idle CPU")
    for result in report["results"]:
        rss = result["resources"]["rss_bytes"]
        rss_text = f"{rss / 1024 / 1024:.2f}" if rss is not None else "n/a"
        print(
            f"{result['panes']:>5}  "
            f"{result['inventory']['median_ms']:.3f}/{result['inventory']['p95_ms']:.3f} ms   "
            f"{result['capture_visible_24']['median_ms']:.3f}/{result['capture_visible_24']['p95_ms']:.3f} ms   "
            f"{result['input_queue_ack']['median_ms']:.3f}/{result['input_queue_ack']['p95_ms']:.3f} ms   "
            f"{rss_text:>7}   "
            f"{result['resources']['idle_cpu_one_core_percent']:.3f}%"
        )


def main():
    if not hasattr(os, "fork") or not hasattr(socket, "AF_UNIX"):
        raise SystemExit("the direct protocol benchmark currently requires Unix local sockets")
    parser = argparse.ArgumentParser()
    parser.add_argument("--luvus", default=str(ROOT / "target" / "release" / "luvus"))
    parser.add_argument("--panes", default="1,10,50")
    parser.add_argument("--samples", type=int, default=50)
    parser.add_argument("--idle-seconds", type=float, default=5.0)
    parser.add_argument("--output", help="optional JSON result path below this checkout")
    args = parser.parse_args()
    if not 1 <= args.samples <= 1000:
        raise SystemExit("--samples must be between 1 and 1000")
    if not 0.1 <= args.idle_seconds <= 60:
        raise SystemExit("--idle-seconds must be between 0.1 and 60")
    try:
        pane_counts = [int(value) for value in args.panes.split(",")]
    except ValueError as error:
        raise SystemExit("--panes must be a comma-separated integer list") from error
    if not pane_counts or any(count < 1 or count > 50 for count in pane_counts):
        raise SystemExit("every pane count must be between 1 and 50")

    binary = pathlib.Path(args.luvus).resolve(strict=True)
    report = {
        "schema": 1,
        "protocol": {"name": "luvus-uhp", "major": 1, "minor": 0},
        "source": {
            "commit": git_value("rev-parse", "HEAD"),
            "dirty": bool(git_value("status", "--porcelain")),
            "binary": str(binary),
            "binary_sha256": binary_digest(binary),
        },
        "host": {
            "system": platform.system(),
            "release": platform.release(),
            "machine": platform.machine(),
            "logical_cpus": os.cpu_count(),
        },
        "samples_per_operation": args.samples,
        "measurement_caveats": [
            "Each request opens one native local-IPC connection and measures server round-trip latency.",
            "Idle CPU is a short process CPU-time delta and is sensitive to timer precision and background work.",
            "RSS is not physical footprint; macOS additionally reports footprint, peak, and live malloc bytes when tools permit.",
            "PTY shell, allocator, filesystem, and host scheduling costs are included.",
        ],
        "results": [],
    }
    for pane_count in pane_counts:
        report["results"].append(
            benchmark_case(binary, pane_count, args.samples, args.idle_seconds)
        )
    print_summary(report)
    encoded = json.dumps(report, indent=2) + "\n"
    if args.output:
        output = pathlib.Path(args.output).resolve()
        try:
            output.relative_to(ROOT)
        except ValueError as error:
            raise SystemExit("--output must stay inside the current checkout") from error
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(encoded)
        print(f"JSON: {output}")
    else:
        print(encoded)


if __name__ == "__main__":
    main()
