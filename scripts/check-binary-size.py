#!/usr/bin/env python3
"""Report release artifact sizes and guard reviewed per-target baselines."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import sys


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", required=True)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--archive", required=True, type=Path)
    parser.add_argument(
        "--baseline",
        default=Path(".github/binary-size-baseline.json"),
        type=Path,
    )
    parser.add_argument(
        "--informational",
        action="store_true",
        help="report growth without failing, for reviewed security updates",
    )
    return parser.parse_args()


def percent_change(actual: int, baseline: int) -> float:
    return (actual - baseline) * 100.0 / baseline


def main() -> int:
    args = parse_args()
    for label, path in (("binary", args.binary), ("archive", args.archive)):
        if not path.is_file():
            print(f"missing {label} artifact: {path}", file=sys.stderr)
            return 2
    config = json.loads(args.baseline.read_text(encoding="utf-8"))
    try:
        expected = config["targets"][args.target]
    except KeyError:
        print(f"missing binary-size baseline for {args.target}", file=sys.stderr)
        return 2

    warn_at = float(config["thresholds"]["warn_percent"])
    block_at = float(config["thresholds"]["block_percent"])
    measurements = (
        ("Executable", args.binary.stat().st_size, int(expected["executable_bytes"])),
        ("Archive", args.archive.stat().st_size, int(expected["archive_bytes"])),
    )
    rows = [
        f"### Binary size for `{args.target}`",
        "",
        "| Artifact | Current | Baseline | Change |",
        "| --- | ---: | ---: | ---: |",
    ]
    blocked = False
    for label, actual, baseline in measurements:
        change = percent_change(actual, baseline)
        rows.append(f"| {label} | {actual:,} B | {baseline:,} B | {change:+.2f}% |")
        message = (
            f"{args.target} {label.lower()} is {change:+.2f}% versus its "
            f"reviewed baseline ({actual} versus {baseline} bytes)"
        )
        if change > block_at and not args.informational:
            print(f"::error title=Binary size exceeded {block_at:g}%::{message}")
            blocked = True
        elif change > warn_at:
            print(f"::warning title=Binary size exceeded {warn_at:g}%::{message}")

    report = "\n".join(rows) + "\n"
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with Path(summary).open("a", encoding="utf-8") as output:
            output.write(report)
    else:
        print(report, end="")
    return 1 if blocked else 0


if __name__ == "__main__":
    raise SystemExit(main())
