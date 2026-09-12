#!/usr/bin/env python3
"""Isolated graphics delivery benchmark, not a host-terminal rendering benchmark.

Uses the version-7 binary display transport with a synthetic graphics-capable
receiver. Measures producer-to-receiver delivery, server CPU/RSS and idle cost.
No terminal GPU, PNG decoding or physical screen latency is measured.
"""

import argparse
import base64
import concurrent.futures
import hashlib
import json
import os
import pathlib
import platform
import select
import signal
import socket
import struct
import subprocess
import sys
import threading
import time

from benchmark import binary_digest, git_value, latency_summary, process_cpu_seconds, process_rss_bytes
from consumer import request
from live_support import ROOT, isolated_server


def uint(value):
    if value < 251:
        return bytes([value])
    if value <= 65535:
        return b"\xfb" + struct.pack("<H", value)
    return b"\xfc" + struct.pack("<I", value)


def number(data, offset=0):
    tag = data[offset]
    if tag < 251:
        return tag, offset + 1
    formats = {251: ("<H", 2), 252: ("<I", 4), 253: ("<Q", 8)}
    fmt, size = formats[tag]
    return struct.unpack_from(fmt, data, offset + 1)[0], offset + size + 1


class Display:
    def __init__(self, state, side, supported=True):
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.settimeout(20)
        self.socket.connect(str(state / "luvus-client.sock"))
        self.condition = threading.Condition()
        self.completed = {}
        self.error = None
        self.frames = 0
        self.graphics_bytes = 0
        self.current_image = None
        self.image_data = bytearray()
        pixels = (bytes(range(256)) * ((side * side * 3 + 255) // 256))[:side * side * 3]
        self.expected_digest = hashlib.sha256(pixels).digest()
        self.supported = supported
        self.send(uint(0) + uint(18) + uint(240) + uint(80))
        self.thread = threading.Thread(target=self.receive, daemon=True)
        self.thread.start()

    def send(self, data):
        self.socket.sendall(struct.pack("<I", len(data)) + data)

    def exact(self, size):
        result = bytearray()
        while len(result) < size:
            chunk = self.socket.recv(size - len(result))
            if not chunk:
                raise EOFError("display disconnected")
            result.extend(chunk)
        return bytes(result)

    def receive(self):
        try:
            while True:
                size = struct.unpack("<I", self.exact(4))[0]
                if size > 64 * 1024 * 1024:
                    raise RuntimeError("oversized display message")
                data = self.exact(size)
                kind, offset = number(data)
                if kind == 1:
                    version, offset = number(data, offset)
                    if version != 18 or data[offset] != 0:
                        raise RuntimeError("expected protocol 18 welcome without error")
                elif kind == 12:
                    # TerminalProbe: no colors, Some(graphics), Some(8x16 cell).
                    self.send(uint(12) + b"\x00\x01" + bytes([self.supported]) + b"\x01" + uint(8) + uint(16))
                    self.send(uint(13) + uint(8) + uint(16))
                elif kind in (2, 3):
                    self.frames += 1
                elif kind == 4:
                    count, offset = number(data, offset)
                    for _ in range(count):
                        length, offset = number(data, offset)
                        command = data[offset:offset + length]
                        offset += length
                        self.graphics_bytes += length
                        control, _, body = command[3:-2].partition(b";")
                        fields = dict(pair.split(b"=", 1) for pair in control.split(b",") if b"=" in pair)
                        if fields.get(b"a", b"T") == b"d":
                            continue
                        if b"i" in fields:
                            if self.current_image is not None:
                                raise RuntimeError(f"interleaved/incomplete image transfer: image {self.current_image}, {len(self.image_data)} base64 bytes before new image {fields[b'i'].decode()}; control={control!r}")
                            self.current_image = int(fields[b"i"])
                            self.image_data.clear()
                        self.image_data.extend(body)
                        if fields.get(b"m", b"0") == b"0":
                            received_at = time.monotonic_ns()
                            decoded = base64.b64decode(self.image_data, validate=True)
                            if hashlib.sha256(decoded).digest() != self.expected_digest:
                                raise RuntimeError("delivered image payload does not match producer")
                            with self.condition:
                                self.completed[self.current_image] = received_at
                                self.condition.notify_all()
                            self.current_image = None
        except Exception as error:
            with self.condition:
                self.error = error
                self.condition.notify_all()

    def wait(self, ids):
        deadline = time.monotonic() + 20
        with self.condition:
            while not all(image in self.completed for image in ids):
                if self.error:
                    raise self.error
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError(f"missing graphics completions: {ids}")
                self.condition.wait(remaining)
            return [self.completed.pop(image) for image in ids]

    def close(self):
        self.socket.shutdown(socket.SHUT_RDWR)
        self.socket.close()
        self.thread.join(timeout=3)


def producer(path, side):
    # Deterministic uncompressed RGB avoids varying compression ratios.
    pixels = (bytes(range(256)) * ((side * side * 3 + 255) // 256))[:side * side * 3]
    encoded = base64.b64encode(pixels)
    listener = socket.socket(socket.AF_UNIX)
    listener.bind(path)
    listener.listen(1)
    connection, _ = listener.accept()
    with connection, connection.makefile("rwb", buffering=0) as stream:
        for line in stream:
            image, row = map(int, line.split())
            chunks = []
            for offset in range(0, len(encoded), 4096):
                more = int(offset + 4096 < len(encoded))
                fields = f"a=T,f=24,s={side},v={side},i={image},c=8,r=4,C=1,q=2,m={more}" if offset == 0 else f"m={more},q=2"
                chunks.append(b"\x1b_G" + fields.encode() + b";" + encoded[offset:offset + 4096] + b"\x1b\\")
            payload = f"\x1b[{row};1H".encode() + b"".join(chunks)
            started = time.monotonic_ns()
            view = memoryview(payload)
            while view:
                view = view[os.write(1, view):]
            stream.write(f"{started}\n".encode())


def api(server, method, params):
    response = request(server.socket_path, {"id": method, "method": method, "params": params})
    if "error" in response:
        raise RuntimeError(response)
    return response["result"]


def resources(server, seconds):
    started = time.monotonic()
    cpu = process_cpu_seconds(server.process.pid)
    time.sleep(seconds)
    elapsed = time.monotonic() - started
    return {"cpu_one_core_percent": 100 * (process_cpu_seconds(server.process.pid) - cpu) / elapsed,
            "rss_bytes": process_rss_bytes(server.process.pid), "window_seconds": elapsed}


def plain_smoke(binary):
    """Exercise the real Luvus client in a PTY that never answers graphics probes."""
    import fcntl
    import pty
    import termios

    with isolated_server(binary, "g") as server:
        pid, master = pty.fork()
        if pid == 0:
            environment = dict(server.environment, TERM="xterm-256color")
            os.execve(str(binary), [str(binary)], environment)
        output = bytearray()
        stopped = threading.Event()
        def drain():
            while not stopped.is_set():
                if select.select([master], [], [], 0.1)[0]:
                    try:
                        chunk = os.read(master, 65536)
                    except OSError:
                        return
                    if not chunk:
                        return
                    output.extend(chunk)
                    if len(output) > 2 * 1024 * 1024:
                        stopped.set()
        thread = threading.Thread(target=drain, daemon=True)
        try:
            fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
            thread.start()
            time.sleep(1)
            capability = api(server, "uhp.capabilities", {})
            if capability["graphics_details"]["available"]:
                raise RuntimeError("plain PTY unexpectedly supports graphics")
            script = "import os,time; os.write(1,b'\\x1b_Ga=T,f=24,s=1,v=1,i=901,c=1,r=1,q=2;AAAA\\x1b\\\\\\r\\nPLAIN-BENCH-OK\\r\\n'); time.sleep(20)"
            created = api(server, "terminal.backend.create", {"cwd": str(ROOT), "command": [sys.executable, "-c", script],
                          "placement": {"kind": "workspace"}, "focus": True, "label": "plain-smoke"})
            deadline = time.monotonic() + 10
            while b"PLAIN-BENCH-OK" not in output:
                if time.monotonic() > deadline:
                    raise RuntimeError("real plain client did not render marker")
                time.sleep(0.05)
            if b"\x1b_Ga=T" in output or "\U0010eeee".encode() in output:
                raise RuntimeError("plain client leaked graphics transmission/placeholders")
            captured = api(server, "terminal.backend.capture", {**{key: created[key] for key in ("server_generation", "terminal_id", "pane_id")}, "mode": "visible", "lines": 40, "ansi": False})
            if "PLAIN-BENCH-OK" not in json.dumps(captured):
                raise RuntimeError("capture did not contain marker")
            return {"status": "passed", "scope": "real client from the selected binary in non-responding PTY, not Terminal.app",
                    "graphics_available": False, "marker_rendered": True, "graphics_leaked": False,
                    "client_output_bytes": len(output)}
        finally:
            stopped.set()
            thread.join(timeout=2)
            os.close(master)
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            os.waitpid(pid, 0)


def case(binary, panes, images, side, samples, idle, mode):
    with isolated_server(binary, "g") as server:
        display = Display(server.state, side)
        connections = []
        try:
            deadline = time.monotonic() + 10
            while not api(server, "uhp.capabilities", {}).get("graphics_details", {}).get("available"):
                if time.monotonic() > deadline:
                    raise RuntimeError("synthetic display did not negotiate graphics")
                time.sleep(0.05)
            initial = api(server, "terminal.backend.inventory", {})
            runtimes = []
            for index in range(panes):
                path = server.state / f"p{index}"
                placement = {"kind": "workspace"} if not runtimes else {"kind": "sibling", "of_terminal": runtimes[(index - 1) // 2]}
                created = api(server, "terminal.backend.create", {
                    "cwd": str(ROOT), "command": [sys.executable, str(pathlib.Path(__file__).resolve()), "--producer", str(path), "--side", str(side)],
                    "placement": placement, "focus": True, "label": f"graphics-bench-{index}"})
                runtimes.append({key: created[key] for key in ("server_generation", "terminal_id", "pane_id")})
                deadline = time.monotonic() + 10
                while not path.exists():
                    if time.monotonic() > deadline:
                        raise RuntimeError("producer did not start")
                    time.sleep(0.02)
                connection = socket.socket(socket.AF_UNIX)
                connection.settimeout(20)
                connection.connect(str(path))
                connections.append(connection.makefile("rwb", buffering=0))
                connection.close()
            for terminal in initial["terminals"]:
                api(server, "terminal.backend.close", {"server_generation": initial["server_generation"], "terminal_id": terminal["terminal_id"], "pane_id": terminal["pane_id"]})
            time.sleep(1)
            baseline = resources(server, idle)
            latencies = []
            batch_latencies = []
            next_id = 100
            cpu_before = process_cpu_seconds(server.process.pid)
            active_start = time.monotonic()
            with concurrent.futures.ThreadPoolExecutor(max_workers=panes) as pool:
                for sample in range(samples + 2):
                    for slot in range(images):
                        ids = list(range(next_id, next_id + panes))
                        next_id += panes
                        def send(item):
                            stream, image = item
                            stream.write(f"{image} {1 + slot * 5}\n".encode())
                            return int(stream.readline())
                        if mode == "concurrent":
                            starts = list(pool.map(send, zip(connections, ids)))
                            ends = display.wait(ids)
                        else:
                            starts, ends = [], []
                            for stream, image in zip(connections, ids):
                                starts.append(send((stream, image)))
                                ends.extend(display.wait([image]))
                        if sample >= 2:
                            latencies.extend((end - start) / 1e6 for start, end in zip(starts, ends))
                            batch_latencies.append((max(ends) - min(starts)) / 1e6)
            active_seconds = time.monotonic() - active_start
            active_cpu = process_cpu_seconds(server.process.pid) - cpu_before
            time.sleep(1)
            after = resources(server, idle)
            return {"status": "passed", "mode": mode, "panes": panes, "images_per_pane_per_sample": images, "side_px": side,
                    "raw_image_bytes": side * side * 3, "samples": samples, "warmup_samples": 2,
                    "delivery": latency_summary(latencies), "batch_delivery": latency_summary(batch_latencies),
                    "active_cpu_one_core_percent": 100 * active_cpu / active_seconds,
                    "active_seconds_including_warmup": active_seconds,
                    "baseline": baseline, "after_images": after,
                    "graphics_bytes_received": display.graphics_bytes, "frames_received": display.frames}
        finally:
            for connection in connections:
                connection.close()
            display.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--producer")
    parser.add_argument("--plain-smoke", action="store_true")
    parser.add_argument("--side", type=int, default=256)
    parser.add_argument("--luvus", default=str(ROOT / "target/release/luvus"))
    parser.add_argument("--samples", type=int, default=10)
    parser.add_argument("--idle-seconds", type=float, default=3)
    parser.add_argument("--panes", default="1,2,4,8")
    parser.add_argument("--images", type=int, default=1)
    parser.add_argument("--mode", choices=("concurrent", "serial"), default="concurrent")
    parser.add_argument("--output", required=False)
    args = parser.parse_args()
    if not 1 <= args.side <= 1024 or not 1 <= args.samples <= 100 or not 1 <= args.images <= 8 or not 0.1 <= args.idle_seconds <= 60:
        parser.error("side 1..1024, samples 1..100, images 1..8, idle-seconds 0.1..60 required")
    if args.producer:
        producer(args.producer, args.side)
        return
    panes = [int(value) for value in args.panes.split(",")]
    if any(value not in (1, 2, 4, 8) for value in panes):
        parser.error("panes must be drawn from 1,2,4,8")
    binary = pathlib.Path(args.luvus).resolve(strict=True)
    output = pathlib.Path(args.output).resolve() if args.output else None
    if output and not output.is_relative_to(ROOT):
        parser.error("output must be inside this checkout")
    report = {"scope": "synthetic protocol-18 receiver; server delivery only, no host rendering",
              "revision": git_value("rev-parse", "HEAD"), "dirty": git_value("status", "--short"),
              "binary_sha256": binary_digest(binary), "harness_sha256": binary_digest(pathlib.Path(__file__)),
              "platform": platform.platform(), "viewport_cells": [240, 80], "cell_pixels": [8, 16],
              "workload": "fresh image IDs; 8x4-cell placements; uncompressed RGB inline chunks <=4096 base64 bytes",
              "results": []}
    if args.plain_smoke:
        report["scope"] = "real client fallback smoke in isolated PTY"
        report["viewport_cells"] = [120, 40]
        report["cell_pixels"] = None
        report["workload"] = "one inline 1x1 RGB image followed by a text marker"
        report["results"].append(plain_smoke(binary))
        print(json.dumps(report), flush=True)
        if output:
            output.write_text(json.dumps(report, indent=2) + "\n")
        return
    for count in panes:
        try:
            result = case(binary, count, args.images, args.side, args.samples, args.idle_seconds, args.mode)
        except Exception as error:
            result = {"status": "failed", "mode": args.mode, "panes": count, "side_px": args.side,
                      "images_per_pane_per_sample": args.images, "error": str(error)}
        report["results"].append(result)
        print(json.dumps(result), flush=True)
        if output:
            output.write_text(json.dumps(report, indent=2) + "\n")
    if any(result["status"] == "failed" for result in report["results"]):
        raise SystemExit(1)


if __name__ == "__main__":
    main()
