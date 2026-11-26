#!/usr/bin/env python3
"""Compare a running minisolr-rs with Python minisolr, request by request, on the captured /select requests.

Python minisolr runs in-process (so run this with its environment), on a copy of the dev data:

    uv run --project ../minisolr python tools/compare_python.py http://127.0.0.1:8983/solr/openlibrary ../minisolr
    ... --neutral     # Python's function boosts evaluate to 1 (to separate parser from boost differences)

Only requests the Rust server answers (no `warning`) are compared. Prints each request whose top-10 keys
differ, then the top-10 overlap of both with real Solr's golden answers and how often numFound, the top-10 set
and the top-10 order are identical.
"""

import json
import logging
import shutil
import sys
import tempfile
import urllib.parse
import urllib.request
from pathlib import Path

sys.dont_write_bytecode = True


def main() -> None:
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    neutral = "--neutral" in sys.argv
    url, minisolr = args[0].rstrip("/"), Path(args[1]).resolve()
    sys.path.insert(0, str(minisolr))
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    logging.disable(logging.WARNING)
    from minisolr import search
    from minisolr.schema import Schema
    from minisolr.store import Store
    from replay import DROP_PARAMS, select_requests

    fx = minisolr / "fixtures"
    tmp = Path(tempfile.mkdtemp())
    shutil.copy(fx / "docs.jsonl", tmp / "docs.jsonl")
    store = Store(Schema(str(minisolr / "conf")), tmp)
    gold = json.loads((fx / "golden.json").read_text())["select"]
    reqs = [r for r in select_requests(json.loads((fx / "requests.json").read_text())) if json.dumps(r["params"]) in gold]
    if neutral:
        search.run_query.__globals__["eval_fn"] = lambda e, d, s=0.0: 1.0

    def rust(params: list[tuple[str, str]]) -> dict:
        data = urllib.parse.urlencode(params).encode()
        with urllib.request.urlopen(urllib.request.Request(url + "/select", data=data), timeout=60) as r:
            return json.loads(r.read())

    same_nf = same_order = same_set = n = py_ov = rs_ov = gk = 0
    for r in reqs:
        params = [(k, v) for k, v in r["params"] if k not in DROP_PARAMS]
        j = rust(params)
        if j["responseHeader"].get("warning"):
            continue
        n += 1
        py = search.select(store, params)
        rk = [d.get("key") for d in j["response"]["docs"]][:10]
        pk = [d.get("key") for d in py["response"]["docs"]][:10]
        g = gold[json.dumps(r["params"])]["keys"]
        py_ov += len(set(pk) & set(g))
        rs_ov += len(set(rk) & set(g))
        gk += len(g)
        same_nf += j["response"]["numFound"] == py["response"]["numFound"]
        same_set += set(rk) == set(pk)
        same_order += rk == pk
        if rk != pk:
            d = dict(r["params"])
            what = "set-equal" if set(rk) == set(pk) else f"overlap {len(set(rk) & set(pk))}/{len(pk)}"
            print("DIFF", d.get("ol.label"), repr((d.get("userWorkQuery") or d.get("q") or "")[:60]), what)
    shutil.rmtree(tmp, ignore_errors=True)
    print(f"overlap with golden: python {py_ov}/{gk} rust {rs_ov}/{gk}")
    print(
        f"compared {n}: numFound equal {same_nf}, top-10 set equal {same_set}, top-10 order equal {same_order} "
        f"(python boosts {'neutralized' if neutral else 'on'})"
    )


if __name__ == "__main__":
    main()
