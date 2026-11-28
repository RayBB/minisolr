#!/usr/bin/env python3
"""Compare a running minisolr-rs with Python minisolr, request by request, on the captured /select requests.

Python minisolr runs in-process (so run this with its environment), on a copy of the dev data:

    uv run --project ../minisolr python tools/compare_python.py http://127.0.0.1:8983/solr/openlibrary .
    ... --neutral     # Python's function boosts evaluate to 1 (to separate parser from boost differences)

Only requests the Rust server answers (no `warning`) are compared. Prints each request whose top-10 keys
differ, then the top-10 overlap of both with real Solr's golden answers and how often numFound, the top-10 set
and the top-10 order are identical, and how often the whole `facet_counts`, `highlighting` and `spellcheck`
sections are. Then the select and JSON probes of e2e/probes.py (no updates): numFound, keys, `start`,
`nextCursorMark`, `facet_counts`, `facets`, `highlighting` and `spellcheck` in full.
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

    same_nf = same_order = same_set = n = py_ov = rs_ov = gk = fac_n = fac_same = 0
    sections = {k: [0, 0] for k in ("highlighting", "spellcheck")}
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
        if "facet_counts" in py or "facet_counts" in j:
            fac_n += 1
            if j.get("facet_counts") == py.get("facet_counts"):
                fac_same += 1
            else:
                print("FACETS DIFF", dict(r["params"]).get("ol.label"))
        for k, tally in sections.items():
            if k in py or k in j:
                tally[0] += 1
                if j.get(k) == py.get(k):
                    tally[1] += 1
                else:
                    print(k.upper(), "DIFF", dict(r["params"]).get("ol.label"), json.dumps(j.get(k))[:300], "python:", json.dumps(py.get(k))[:300])
        if rk != pk:
            d = dict(r["params"])
            what = "set-equal" if set(rk) == set(pk) else f"overlap {len(set(rk) & set(pk))}/{len(pk)}"
            print("DIFF", d.get("ol.label"), repr((d.get("userWorkQuery") or d.get("q") or "")[:60]), what)
    print(f"facet_counts identical: {fac_same}/{fac_n}")
    for k, (tn, ts) in sections.items():
        print(f"{k} identical: {ts}/{tn}")
    probes(url, minisolr, store, search)
    shutil.rmtree(tmp, ignore_errors=True)
    print(f"overlap with golden: python {py_ov}/{gk} rust {rs_ov}/{gk}")
    print(
        f"compared {n}: numFound equal {same_nf}, top-10 set equal {same_set}, top-10 order equal {same_order} "
        f"(python boosts {'neutralized' if neutral else 'on'})"
    )


def probes(url: str, minisolr: Path, store: object, search: object) -> None:
    sys.path.insert(0, str(minisolr / "e2e"))
    import probes as probe_defs

    def essence(j: dict) -> dict:
        resp = j.get("response", {})
        return {
            "numFound": resp.get("numFound"),
            "start": resp.get("start"),
            "keys": [d.get("key") for d in resp.get("docs", [])],
            **{k: j.get(k) for k in ("nextCursorMark", "facet_counts", "facets", "highlighting", "spellcheck")},
        }

    same = n = 0
    for p in probe_defs.PROBES:
        kind = p.get("kind", "select")
        if kind == "select":
            pairs = [(k, str(x)) for k, v in p["params"].items() for x in (v if isinstance(v, list) else [v])]
            req = urllib.request.Request(url + "/select", data=urllib.parse.urlencode(pairs).encode())
            py = search.select(store, pairs)
        elif kind == "json":
            req = urllib.request.Request(url + "/query", data=json.dumps(p["body"]).encode(), headers={"Content-Type": "application/json"})
            py = search.select_json(store, p["body"], [])
        else:
            continue
        with urllib.request.urlopen(req, timeout=60) as r:
            j = json.loads(r.read())
        if j["responseHeader"].get("warning"):
            continue
        n += 1
        if essence(j) == essence(py):
            same += 1
        else:
            print("PROBE DIFF", p["name"], json.dumps(essence(j))[:300], "python:", json.dumps(essence(py))[:300])
    print(f"select and JSON probes identical to Python (incl. start, cursor, facets, highlighting, spellcheck): {same}/{n}")


if __name__ == "__main__":
    main()
