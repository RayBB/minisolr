#!/usr/bin/env python3
"""Run all of Python minisolr's e2e feature probes (e2e/probes.py) against a minisolr-rs server, as the Docker harness does.

    python3 tools/probes_compare.py . --binary target/release/minisolr-rs   # starts its own server
    python3 tools/probes_compare.py . --url http://127.0.0.1:8983/solr/openlibrary

It imports `e2e/probes.py`, points `probes.BASE` at the server and calls `probes.compare()`, which runs every probe
in order (the update lifecycle probes add and then delete a document) and compares with
`fixtures/probes_golden.json` under its hard-check rules, printing `probes: N/66 match real Solr (hard checks); ...`.
`probes.setup()` is not needed: `fixtures/docs.jsonl` already holds the list and trending state it creates.

With --binary the server is started on a free port from a copy of `fixtures/docs.jsonl` in a temporary data
directory, and stopped and removed afterwards; with --url the probes mutate that server's data (the lifecycle
leaves it as it found it). The result file probes.py writes goes to a temporary directory, not into minisolr.
Exits 0 when no probe fails a hard check. Standard library only.
"""

import argparse
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

sys.dont_write_bytecode = True


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def wait_ready(base: str, proc: subprocess.Popen[bytes], timeout: float = 30) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if proc.poll() is not None:
            raise SystemExit(f"server exited with status {proc.returncode}")
        try:
            with urllib.request.urlopen(base + "/admin/ping", timeout=2):
                return
        except OSError:
            time.sleep(0.2)
    raise SystemExit("server did not answer /admin/ping")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("minisolr", type=Path, help="Python minisolr checkout (conf/, fixtures/, e2e/)")
    group = ap.add_mutually_exclusive_group(required=True)
    group.add_argument("--binary", type=Path, help="minisolr-rs binary to start on a copy of the fixtures")
    group.add_argument("--url", help="base URL of a running server, e.g. http://127.0.0.1:8983/solr/openlibrary")
    args = ap.parse_args()
    minisolr = args.minisolr.resolve()
    sys.path.insert(0, str(minisolr / "e2e"))
    import probes  # minisolr's e2e/probes.py: definitions and compare(); nothing runs on import

    tmp = Path(tempfile.mkdtemp(prefix="minisolr-probes-"))
    probes.LAST = tmp / "last_probes.json"
    proc = None
    try:
        if args.binary:
            data = tmp / "data"
            data.mkdir()
            shutil.copy(minisolr / "fixtures" / "docs.jsonl", data / "docs.jsonl")
            port = free_port()
            base = f"http://127.0.0.1:{port}/solr/openlibrary"
            log = (tmp / "server.log").open("wb")
            proc = subprocess.Popen([str(args.binary), "serve", str(minisolr / "conf"), str(data), f"127.0.0.1:{port}"], stdout=log, stderr=log)
            wait_ready(base, proc)
        else:
            base = args.url.rstrip("/")
        probes.BASE = base
        ok = probes.compare()
    finally:
        if proc is not None:
            proc.terminate()
            proc.wait(timeout=10)
        shutil.rmtree(tmp, ignore_errors=True)
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
