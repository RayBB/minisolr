#!/usr/bin/env python3
"""Pass/fail gate for CI: start the release binary on the dev set, replay the captured Open Library
requests against it and run the feature probes. Standard library only.

    cargo build --release && python3 tools/check.py
"""

from __future__ import annotations

import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "target" / "release" / "minisolr-rs"
MIN_OVERLAP_PERCENT = 98.0


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def ratio(out: str, label: str) -> tuple[int, int]:
    m = re.search(rf"^{re.escape(label)}: (\d+)/(\d+)", out, re.MULTILINE)
    if not m:
        raise SystemExit(f"replay output has no line for {label!r}:\n{out}")
    return int(m[1]), int(m[2])


def main() -> int:
    tmp = Path(tempfile.mkdtemp(prefix="minisolr-rs-check-"))
    (tmp / "data").mkdir()
    shutil.copy(ROOT / "fixtures" / "docs.jsonl", tmp / "data" / "docs.jsonl")
    port = free_port()
    base = f"http://127.0.0.1:{port}/solr/openlibrary"
    proc = subprocess.Popen([str(BINARY), "serve", str(ROOT / "conf"), str(tmp / "data"), f"127.0.0.1:{port}"])  # noqa: S603
    try:
        for _ in range(150):
            try:
                urllib.request.urlopen(base + "/admin/ping", timeout=2).close()  # noqa: S310
                break
            except OSError:
                time.sleep(0.2)
        else:
            print("server did not answer /admin/ping")
            return 1
        replay = subprocess.run([sys.executable, str(ROOT / "tools" / "replay.py"), base, str(ROOT / "fixtures")], capture_output=True, text=True, check=False)  # noqa: S603
        print(replay.stdout)
        if replay.returncode:
            print(replay.stderr)
            return 1
        problems = []
        for label in ("numFound equal", "facets (top 5 per facet field) equal", "/get equal"):
            got, total = ratio(replay.stdout, label)
            if got != total or total == 0:
                problems.append(f"{label}: {got}/{total}")
        m = re.search(r"top-10 key overlap: \d+/\d+ \(([\d.]+)%\)", replay.stdout)
        if not m or float(m[1]) < MIN_OVERLAP_PERCENT:
            problems.append(f"top-10 overlap below {MIN_OVERLAP_PERCENT}%")
    finally:
        proc.terminate()
        proc.wait(timeout=10)
        shutil.rmtree(tmp, ignore_errors=True)
    if problems:
        print("FAILED:", *problems, sep="\n  ")
        return 1
    probes = subprocess.run([sys.executable, str(ROOT / "tools" / "probes_compare.py"), str(ROOT), "--binary", str(BINARY)], check=False)  # noqa: S603
    return probes.returncode


if __name__ == "__main__":
    sys.exit(main())
