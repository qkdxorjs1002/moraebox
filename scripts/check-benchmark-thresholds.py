#!/usr/bin/env python3
"""Fail when a morae benchmark JSON report exceeds configured limits."""

import json
import sys
from pathlib import Path


def load_json(path: str) -> dict:
    if path == "-":
        return json.load(sys.stdin)
    with Path(path).open(encoding="utf-8") as source:
        return json.load(source)


def main() -> int:
    if len(sys.argv) != 3:
        print(
            "usage: check-benchmark-thresholds.py REPORT.json THRESHOLDS.json",
            file=sys.stderr,
        )
        return 2

    report = load_json(sys.argv[1])
    limits = load_json(sys.argv[2])
    failures: list[str] = []

    checks = (
        (
            report.get("failures", 0) <= limits["max_failures"],
            f"failures {report.get('failures')} > {limits['max_failures']}",
        ),
        (
            report.get("throughput_runs_per_second", 0)
            >= limits["min_throughput_runs_per_second"],
            "throughput_runs_per_second "
            f"{report.get('throughput_runs_per_second')} < "
            f"{limits['min_throughput_runs_per_second']}",
        ),
    )
    failures.extend(message for passed, message in checks if not passed)

    for phase_name, limit_name in (
        ("first_output", "max_first_output_p95_micros"),
        ("full_completion", "max_full_completion_p95_micros"),
    ):
        phase = report.get(phase_name)
        if not phase:
            failures.append(f"{phase_name} summary is missing")
        elif phase.get("p95_micros", sys.maxsize) > limits[limit_name]:
            failures.append(
                f"{phase_name}.p95_micros {phase.get('p95_micros')} > "
                f"{limits[limit_name]}"
            )

    peak_rss = report.get("peak_child_rss_bytes")
    if peak_rss is not None and peak_rss > limits["max_peak_child_rss_bytes"]:
        failures.append(
            f"peak_child_rss_bytes {peak_rss} > "
            f"{limits['max_peak_child_rss_bytes']}"
        )

    if failures:
        print("benchmark regression detected:", file=sys.stderr)
        for failure in failures:
            print(f"- {failure}", file=sys.stderr)
        return 1
    print("benchmark thresholds satisfied")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
