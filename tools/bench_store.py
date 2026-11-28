#!/usr/bin/env python3
"""Time start-up, updates and the first search after an update on a data directory (standard library only).

    python3 tools/gen_docs.py 10000 /tmp/ms10k/docs.jsonl
    python3 tools/bench_store.py target/release/minisolr conf /tmp/ms10k

Starts the server on the directory (it is modified: updates are appended or rewritten there), then prints:
cold start until the ping answers, RSS, the first search (index build), one single-document update, the first
search after it, and 50 single-document update requests followed by the first search.
"""

import json
import subprocess
import sys
import time
import urllib.parse
import urllib.request

PORT = 8997
BASE = f"http://127.0.0.1:{PORT}/solr/openlibrary"


def call(path: str, data: bytes | None = None, ctype: str = "application/json") -> bytes:
    req = urllib.request.Request(BASE + path, data=data, headers={"Content-Type": ctype} if data else {})
    with urllib.request.urlopen(req, timeout=300) as resp:
        return resp.read()


def timed(fn) -> float:
    t = time.perf_counter()
    fn()
    return time.perf_counter() - t


def search() -> None:
    call("/select?" + urllib.parse.urlencode({"q": "title:alice", "rows": "1"}))


def update(i: int) -> None:
    doc = {"key": f"/works/BENCH{i}W", "type": "work", "title": f"bench work {i}", "edition_count": i}
    call("/update?commit=true", json.dumps({"add": {"doc": doc}, "commit": {}}).encode())


def rss_mib(pid: int) -> float:
    return int(subprocess.check_output(["ps", "-o", "rss=", "-p", str(pid)]).split()[0]) / 1024


def main() -> None:
    binary, conf, data = sys.argv[1:4]
    t0 = time.perf_counter()
    proc = subprocess.Popen([binary, "serve", conf, data, f"127.0.0.1:{PORT}"], stderr=subprocess.DEVNULL)
    try:
        while True:
            try:
                call("/admin/ping")
                break
            except OSError:
                time.sleep(0.01)
        cold = time.perf_counter() - t0
        first = timed(search)
        r = {
            "cold start s": cold,
            "RSS after first search MiB": rss_mib(proc.pid),
            "first search (index build) s": first,
            "single update s": timed(lambda: update(0)),
            "first search after update s": timed(search),
        }
        r["50 updates s"] = timed(lambda: [update(i) for i in range(1, 51)])
        r["first search after 50 updates s"] = timed(search)
        r["RSS at end MiB"] = rss_mib(proc.pid)
        for k, v in r.items():
            print(f"{k}: {v:.3f}")
    finally:
        proc.terminate()
        proc.wait()


if __name__ == "__main__":
    main()
