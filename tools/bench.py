#!/usr/bin/env python3
"""Measure a running server's memory and latency over a replayed request set (standard library only).

    python3 tools/replay.py URL fixtures --subset-out answered.json
    python3 tools/bench.py URL PID answered.json 1300

Prints RSS (`ps -o rss=`) and macOS `footprint` after one warm-up query and again after N queries
(cycling through the subset), plus latency p50/p95. `footprint` is steadier than RSS on a Mac under
memory pressure; it prints `nan` where `footprint` is unavailable.
"""

import json
import statistics
import subprocess
import sys
import time
import urllib.parse
import urllib.request


def post(url: str, params: list[tuple[str, str]]) -> float:
    data = urllib.parse.urlencode(params).encode()
    t = time.perf_counter()
    with urllib.request.urlopen(urllib.request.Request(url.rstrip("/") + "/select", data=data), timeout=60) as resp:
        resp.read()
    return (time.perf_counter() - t) * 1000


def rss_mib(pid: int) -> float:
    return int(subprocess.check_output(["ps", "-o", "rss=", "-p", str(pid)]).split()[0]) / 1024


def footprint_mib(pid: int) -> float:
    try:
        out = subprocess.run(["footprint", "-p", str(pid)], capture_output=True, text=True, check=False).stdout
    except FileNotFoundError:
        return float("nan")
    for line in out.splitlines():
        if "Footprint:" in line:
            num, unit = line.split("Footprint:")[1].split()[:2]
            return float(num) * {"KB": 1 / 1024, "MB": 1, "GB": 1024}[unit]
    return float("nan")


def main() -> None:
    url, pid, subset_path, n = sys.argv[1], int(sys.argv[2]), sys.argv[3], int(sys.argv[4])
    with open(subset_path) as f:
        subset = [[tuple(kv) for kv in s] for s in json.load(f)]
    post(url, subset[0])
    idle, idle_fp = rss_mib(pid), footprint_mib(pid)
    lat = sorted(post(url, subset[i % len(subset)]) for i in range(n))
    after, after_fp = rss_mib(pid), footprint_mib(pid)
    print(
        f"RSS after 1 query {idle:.1f} MiB (footprint {idle_fp:.1f}); after {n} queries {after:.1f} MiB (footprint {after_fp:.1f}); "
        f"subset {len(subset)} requests; latency p50 {statistics.median(lat):.2f} ms p95 {lat[int(len(lat) * 0.95)]:.2f} ms"
    )


if __name__ == "__main__":
    main()
