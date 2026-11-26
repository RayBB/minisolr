#!/usr/bin/env python3
"""Reindex a server through /update in Open Library's payload shape, and compare two servers.

Standard library only; it starts nothing.

    # rebuild every work with its editions nested, as repeated `add` commands plus `commit`
    python3 tools/roundtrip.py load http://127.0.0.1:8984/solr/openlibrary ../minisolr/fixtures/docs.jsonl [--batch 50]
    # same answers? every captured /select and /get, and every stored row through /export
    python3 tools/roundtrip.py compare http://127.0.0.1:8983/solr/openlibrary http://127.0.0.1:8984/solr/openlibrary ../minisolr/fixtures

`load` groups the flat rows of a minisolr docs.jsonl by `_root_`: each root row becomes one document
(without `_root_`) whose children (without `_root_` and `_nest_path_`, in file order) go in its
`editions` array, which is how Open Library's solr-updater sends works. Each batch is one body with a
repeated `add` key per document and a final `commit`, so a JSON parser that collapses repeated keys
would keep only the last document of each batch. The target should start empty.

`compare` replays the captured selects (as tools/replay.py picks them) and gets on both servers and
counts responses whose `numFound` and full `docs` list (order included) are identical, then compares
every row returned by `/export?q=*:*&fl=*`.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any
from urllib.request import Request, urlopen

sys.path.insert(0, str(Path(__file__).resolve().parent))
from replay import DROP_PARAMS, post, select_requests  # noqa: E402


def families(path: Path) -> list[dict[str, Any]]:
    roots: dict[str, dict[str, Any]] = {}
    for line in path.read_text().splitlines():
        if not line.strip():
            continue
        row = json.loads(line)
        root = row.pop("_root_", None) or row["key"]
        row.pop("_nest_path_", None)
        family = roots.setdefault(root, {"editions": []})
        if root == row["key"]:
            family.update(row)
        else:
            family["editions"].append(row)
    return list(roots.values())


def load(base: str, path: Path, batch: int) -> None:
    docs = families(path)
    sent = 0
    for i in range(0, len(docs), batch):
        part = docs[i : i + batch]
        body = "{" + ", ".join('"add": ' + json.dumps({"doc": d}) for d in part) + ', "commit": {}}'
        req = Request(base.rstrip("/") + "/update?update.partial.requireInPlace=true", data=body.encode(), headers={"Content-Type": "application/json"})
        with urlopen(req, timeout=120) as resp:
            out = json.loads(resp.read())
        if out.get("responseHeader", {}).get("status") != 0:
            raise SystemExit(f"batch {i // batch}: {out}")
        sent += len(part)
    rows = sum(1 + len(d.get("editions", [])) for d in docs)
    with urlopen(base.rstrip("/").rsplit("/", 1)[0] + "/admin/cores?action=STATUS", timeout=60) as resp:
        num = json.loads(resp.read())["status"]["openlibrary"]["index"]["numDocs"]
    print(f"sent {sent} documents ({rows} rows with editions) in {(len(docs) + batch - 1) // batch} batches; server has {num} rows")


def compare(a: str, b: str, fixtures: Path) -> None:
    reqs = json.loads((fixtures / "requests.json").read_text())
    same = total = 0
    for r in select_requests(reqs):
        params = [(k, v) for k, v in r["params"] if k not in DROP_PARAMS]
        ja, jb = post(a, params), post(b, params)
        total += 1
        ra, rb = ja.get("response", {}), jb.get("response", {})
        if ra.get("numFound") == rb.get("numFound") and ra.get("docs") == rb.get("docs") and ja.get("responseHeader", {}).get("warning") == jb.get("responseHeader", {}).get("warning"):
            same += 1
        else:
            print("  SELECT DIFFERS", dict(r["params"]).get("ol.label"), (dict(r["params"]).get("q") or "")[:80])
    print(f"selects identical (numFound, docs in order, warning): {same}/{total}")
    gets = list({json.dumps(r["params"]): r for r in reqs if r["path"] == "/get"}.values())
    same = sum(post(a, [(k, v) for k, v in r["params"]], "/get") == post(b, [(k, v) for k, v in r["params"]], "/get") for r in gets)
    print(f"gets identical: {same}/{len(gets)}")
    export = [("q", "*:*"), ("fl", "*"), ("sort", "key asc")]
    ea, eb = post(a, export, "/export")["response"], post(b, export, "/export")["response"]
    rows_same = sum(x == y for x, y in zip(ea["docs"], eb["docs"], strict=False))
    print(f"exported rows: {ea['numFound']} vs {eb['numFound']}; identical: {rows_same}")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    lp = sub.add_parser("load", help="send docs.jsonl to an empty server through /update")
    lp.add_argument("base")
    lp.add_argument("docs", type=Path)
    lp.add_argument("--batch", type=int, default=50, help="documents (works with their editions) per request")
    cp = sub.add_parser("compare", help="compare two servers on the captured requests and all rows")
    cp.add_argument("a")
    cp.add_argument("b")
    cp.add_argument("fixtures", type=Path)
    args = ap.parse_args()
    if args.cmd == "load":
        load(args.base, args.docs, args.batch)
    else:
        compare(args.a, args.b, args.fixtures)


if __name__ == "__main__":
    main()
