#!/usr/bin/env python3
"""Smoke-test an Open Library web server. Exits non-zero if any check fails."""

from __future__ import annotations

import argparse
import json
import sys
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import Any

Expect = tuple[str, dict[str, Any]]
Check = tuple[str, Expect]
Result = dict[str, Any]

TIMEOUT = 60
TRACEBACK = "Traceback (most recent call last)"


def ok_status(*, allow404: bool = False) -> Expect:
    return ("ok_status", {"allow404": allow404})


def json_min_docs(n: int) -> Expect:
    return ("json_min_docs", {"n": n})


def json_zero_ok() -> Expect:
    return ("json_zero_ok", {})


def page_contains(text: str, *, allow404: bool = False) -> Expect:
    return ("page_contains", {"text": text, "allow404": allow404})


S = "/search.json?q="

CHECKS: list[Check] = [
    # search.json basics
    (S + "twain", json_min_docs(1)),
    (S + "mark+twain&fields=key,title,author_name,editions", json_min_docs(1)),
    (S + "title:alice", json_min_docs(1)),
    (S + "author_key:OL18319A", json_min_docs(1)),
    (S + "lewis+carroll+alice", json_min_docs(1)),
    (S + "%22tom+sawyer%22", json_min_docs(1)),
    (S + "gulliver%27s+travels", json_min_docs(1)),
    (S + "odyssey+homer", json_min_docs(1)),
    (S + "flatland", json_min_docs(1)),
    (S + "robinson+crusoe", json_min_docs(1)),
    (S + "green+eggs+and+ham", json_min_docs(1)),
    (S + "css3+for+web+designers", json_min_docs(1)),
    (S + "html5+for+web+designers", json_min_docs(1)),
    (S + "three+cups+of+tea", json_min_docs(1)),
    (S + "remix", json_min_docs(1)),
    (S + "responsive+web+design", json_min_docs(1)),
    (S + "eggs&fields=*,editions", json_min_docs(1)),
    (S + "subject:fiction", json_min_docs(1)),
    (S + "subject:%22lending+library%22", json_min_docs(1)),
    (S + "subject:%22web+design%22", json_min_docs(1)),
    (S + "subject:%22accessible+book%22", json_min_docs(1)),
    (S + "edition_key:OL4M", json_min_docs(1)),
    (S + "first_publish_year:[1800+TO+2100]", json_min_docs(1)),
    (S + "*:*&facet=true&fields=key", json_min_docs(1)),
    # phrase + boolean
    (S + "title:%22green+eggs%22+AND+author:seuss", json_min_docs(1)),
    # no-hit and typo queries
    (S + "zzzzqqq", json_zero_ok()),
    (S + "twian", ok_status()),
    (S + "twian&_spellcheck_count=3", ok_status()),
    (S + "alise+in+wonderland&_spellcheck_count=3", ok_status()),
    # sorts
    (S + "web+design&sort=new", json_min_docs(1)),
    (S + "css&sort=old", json_min_docs(1)),
    (S + "web&sort=random", json_min_docs(1)),
    (S + "web&sort=rating", ok_status()),
    (S + "*:*&sort=editions", json_min_docs(1)),
    (S + "web&sort=trending", ok_status()),
    # modes
    (S + "responsive+web+design&mode=everything", json_min_docs(1)),
    (S + "twain&mode=ebooks", ok_status()),
    (S + "twain&mode=printdisabled", ok_status()),
    (S + "twain&mode=everything", json_min_docs(1)),
    # facet filters
    (S + "twain&language=eng", json_min_docs(1)),
    (S + "twain&has_fulltext=true", ok_status()),
    (S + "*:*&language=eng&has_fulltext=true", ok_status()),
    (S + "twain&author_key=OL18319A", ok_status()),
    (S + "twain&author_facet=OL18319A", ok_status()),
    (S + "language:eng&facet=true&fields=key,title", json_min_docs(1)),
    (S + "has_fulltext:true", ok_status()),
    (S + "ebook_access:public", ok_status()),
    # pagination
    (S + "twain&page=2&limit=5", ok_status()),
    (S + "twain&page=1&limit=5", json_min_docs(1)),
    (S + "robinson+crusoe&limit=5&page=1", json_min_docs(1)),
    # authors/subjects/lists search
    ("/search/authors.json?q=twain", json_min_docs(1)),
    ("/search/authors.json?q=lewis", json_min_docs(1)),
    ("/search/authors.json?q=carroll&limit=3", json_min_docs(1)),
    ("/search/authors.json?q=kamala+das", json_min_docs(1)),
    ("/search/authors.json?q=zzzzqqq", json_zero_ok()),
    ("/search/subjects.json?q=fiction", json_min_docs(1)),
    ("/search/subjects.json?q=web", json_min_docs(1)),
    ("/search/subjects.json?q=zzzzqqq", json_zero_ok()),
    ("/search/lists.json?q=a", ok_status()),
    ("/search/inside.json?q=alice", ok_status(allow404=True)),
    # HTML search pages
    ("/search?q=twain", page_contains("Twain")),
    ("/search?q=alice&mode=everything", page_contains("Alice")),
    ("/search?q=css&sort=new", ok_status()),
    ("/search?q=mark+twain&has_fulltext=true", ok_status()),
    ("/search?q=subject%3Afiction", ok_status()),
    ("/search?q=title%3Aflatland", page_contains("Flatland")),
    ("/search?q=the", ok_status()),
    ("/search?q=twian", ok_status()),
    ("/search?q=zzzzqqq", ok_status()),
    ("/search/authors?q=twain", page_contains("Twain")),
    ("/search/subjects?q=fiction", ok_status()),
    # site pages
    ("/", ok_status()),
    ("/lists", ok_status()),
    ("/trending/daily", ok_status()),
    ("/browse", ok_status(allow404=True)),
    ("/subjects/fiction", page_contains("fiction")),
    ("/subjects/web_design", ok_status()),
    ("/subjects/accessible_book", ok_status()),
    ("/subjects/fiction.json", ok_status()),
    ("/publishers/Penguin", ok_status(allow404=True)),
    ("/authors/OL18319A", page_contains("Twain")),
    ("/authors/OL4A", page_contains("Kamala Das")),
    ("/authors/OL4A/works.json", ok_status()),
    ("/works/OL45310W", page_contains("Robinson Crusoe")),
    ("/works/OL45310W/editions.json", ok_status()),
    ("/books/OL4M", ok_status()),
]

RESULT_KEYS = ("numFound", "docs")


def fetch(base: str, path: str) -> tuple[int, str]:
    url = base.rstrip("/") + path
    req = urllib.request.Request(url, headers={"User-Agent": "minisolr-smoke/1.0"})
    try:
        with urllib.request.urlopen(req, timeout=TIMEOUT) as r:
            return r.status, r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace")


def evaluate(kind: str, args: dict[str, Any], status: int, body: str) -> tuple[bool, str]:
    if status >= 500:
        return False, f"HTTP {status}"
    if TRACEBACK in body:
        return False, "python traceback in body"
    allow404 = args.get("allow404", False)
    if status == 404 and allow404:
        return True, "404 allowed"
    if status != 200:
        return False, f"HTTP {status}"
    if kind == "ok_status":
        return True, "HTTP 200"
    if kind == "page_contains":
        if args["text"] in body:
            return True, "contains {!r}".format(args["text"])
        return False, "missing {!r}".format(args["text"])
    try:
        data = json.loads(body)
    except ValueError as e:
        return False, f"invalid JSON: {e}"
    if not isinstance(data, dict):
        return False, "JSON is not an object"
    for k in RESULT_KEYS:
        if k not in data:
            return False, f"missing key {k!r}"
    nf, docs = data["numFound"], data["docs"]
    if kind == "json_zero_ok":
        if nf == 0:
            return True, "numFound=0"
        return False, f"expected numFound=0, got {nf}"
    if kind == "json_min_docs":
        n = args["n"]
        if isinstance(nf, int) and nf >= n and len(docs) >= 1:
            return True, f"numFound={nf} docs={len(docs)}"
        return False, f"numFound={nf} docs={len(docs)} (need >={n}, docs>=1)"
    return False, f"unknown expectation {kind}"


def run_check(base: str, check: Check) -> Result:
    path, (kind, args) = check
    t0 = time.time()
    try:
        status, body = fetch(base, path)
        ok, detail = evaluate(kind, args, status, body)
    except Exception as e:
        ok, detail = False, f"{type(e).__name__}: {e}"
    ms = int((time.time() - t0) * 1000)
    return {"ok": ok, "ms": ms, "path": path, "expect": kind, "detail": detail}


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("base", nargs="?", default="http://localhost:8080")
    ap.add_argument("--only", metavar="SUBSTR", help="run only checks whose path contains SUBSTR")
    ap.add_argument("--json-out", metavar="FILE", help="write results as JSON")
    ap.add_argument("--workers", type=int, default=4)
    opts = ap.parse_args()

    checks = [c for c in CHECKS if not opts.only or opts.only in c[0]]
    with ThreadPoolExecutor(max_workers=max(1, opts.workers)) as ex:
        results = list(ex.map(lambda c: run_check(opts.base, c), checks))

    for r in results:
        print(f"{'PASS' if r['ok'] else 'FAIL'}  {r['ms']}  {r['path']}  {r['detail']}")
    passed = sum(r["ok"] for r in results)
    failed = len(results) - passed
    print(f"{passed} passed, {failed} failed")
    if opts.json_out:
        Path(opts.json_out).write_text(json.dumps({"base": opts.base, "passed": passed, "failed": failed, "results": results}, indent=2))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
