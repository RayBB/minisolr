#!/usr/bin/env python3
"""Replay the captured Open Library /select requests against a running server and compare with real Solr.

Standard library only; it starts nothing. Point it at any Solr-compatible core (minisolr-rs,
Python minisolr or Solr itself) and at minisolr's fixtures directory:

    python3 tools/replay.py http://127.0.0.1:8983/solr/openlibrary ../minisolr/fixtures
    python3 tools/replay.py URL FIXTURES --probes      # also the select-kind feature probes
    python3 tools/replay.py URL FIXTURES -v            # list every miss and unanswered request
    python3 tools/replay.py URL FIXTURES --subset-out answered.json   # params of answerable requests

Requests are chosen as in minisolr's e2e/harness.py `select_requests`: top-level /select with an
`ol.label` param and no `{params...}` keys, de-duplicated by their params; `timeAllowed` is dropped.
A request is answerable when the response has a `response` and no `warning` in `responseHeader`.
"""

from __future__ import annotations

import argparse
import json
import sys
import urllib.error
import urllib.parse
import urllib.request
from collections import Counter
from pathlib import Path
from typing import Any

DROP_PARAMS = {"timeAllowed"}


def select_requests(reqs: list[dict[str, Any]]) -> list[dict[str, Any]]:
    keep = (r for r in reqs if r["path"] == "/select" and "ol.label" in dict(r["params"]) and not any(k.startswith("{params") for k, _ in r["params"]))
    return list({json.dumps(r["params"]): r for r in keep}.values())


def post(base: str, pairs: list[tuple[str, str]]) -> dict[str, Any]:
    data = urllib.parse.urlencode(pairs).encode()
    req = urllib.request.Request(base.rstrip("/") + "/select", data=data)
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            return json.loads(resp.read() or b"{}")
    except urllib.error.HTTPError as e:
        try:
            return json.loads(e.read())
        except Exception:
            return {"error": {"msg": f"HTTP {e.code}"}}
    except Exception as e:
        return {"error": {"msg": f"{type(e).__name__}: {e}"}}


def unanswered_reason(j: dict[str, Any]) -> str | None:
    if "error" in j:
        return "error: " + str(j["error"].get("msg", j["error"]))[:80]
    if warning := j.get("responseHeader", {}).get("warning"):
        return str(warning)
    if "response" not in j:
        return "no response"
    return None


def reason_group(reason: str) -> str:
    """A short bucket for a warning: the warning without the `unsupported:` prefix, cut at 60 characters."""
    return reason.removeprefix("unsupported: ")[:60]


def replay(base: str, fixtures: Path, verbose: bool, subset_out: Path | None) -> None:
    reqs = json.loads((fixtures / "requests.json").read_text())
    gold = json.loads((fixtures / "golden.json").read_text())["select"]
    sels = [r for r in select_requests(reqs) if json.dumps(r["params"]) in gold]
    answerable = nf_equal = set_equal = overlap = gold_keys = 0
    reasons: Counter[str] = Counter()
    misses: list[str] = []
    unanswered: list[str] = []
    subset: list[list[tuple[str, str]]] = []
    for r in sels:
        g = gold[json.dumps(r["params"])]
        params = [(k, v) for k, v in r["params"] if k not in DROP_PARAMS]
        j = post(base, params)
        d = dict(r["params"])
        label = f"{d.get('ol.label')} q={(d.get('userWorkQuery') or d.get('q') or '')[:70]!r}"
        if reason := unanswered_reason(j):
            reasons[reason_group(reason)] += 1
            unanswered.append(f"{label}: {reason[:160]}")
            continue
        answerable += 1
        subset.append(params)
        resp = j["response"]
        keys = [doc.get("key") for doc in resp.get("docs", [])][:10]
        n_ok = resp.get("numFound") == g["numFound"]
        common = len(set(keys) & set(g["keys"]))
        nf_equal += n_ok
        set_equal += set(keys) == set(g["keys"])
        overlap += common
        gold_keys += len(g["keys"])
        if not n_ok or set(keys) != set(g["keys"]):
            misses.append(f"{label} fq={d.get('fq')!r}: numFound {resp.get('numFound')} vs {g['numFound']}, top-10 overlap {common}/{len(g['keys'])}")
    pct = 100.0 * overlap / gold_keys if gold_keys else 0.0
    print(f"requests (deduped, with golden): {len(sels)}")
    print(f"answerable: {answerable}")
    print(f"numFound equal: {nf_equal}/{answerable}")
    print(f"top-10 set equal: {set_equal}/{answerable}")
    print(f"top-10 key overlap: {overlap}/{gold_keys} ({pct:.2f}%)")
    print(f"unanswered: {len(sels) - answerable}")
    for reason, n in reasons.most_common():
        print(f"  {n:4d}  {reason}")
    if verbose:
        for m in misses:
            print("  MISS", m)
        for u in unanswered:
            print("  UNANSWERED", u)
    if subset_out:
        subset_out.write_text(json.dumps(subset))


def probe_essence(p: dict[str, Any], j: dict[str, Any]) -> dict[str, Any]:
    """The select part of e2e/probes.py `norm`: numFound, top-10 keys, docs when `fl` lists fields."""
    resp = j.get("response", {})
    out: dict[str, Any] = {}
    if resp:
        out["numFound"] = resp.get("numFound")
        docs = resp.get("docs", [])
        out["keys"] = [d.get("key") for d in docs][:10]
        fl = p.get("params", {}).get("fl", "")
        if "," in fl and "editions" not in fl and "[child]" not in fl:
            out["docs"] = [{k: v for k, v in d.items() if k != "score"} for d in docs[:10]]
    return out


def probes(base: str, fixtures: Path, verbose: bool) -> None:
    """Select-kind probes against probes_golden.json, with the hard checks of e2e/probes.py `compare`.

    The golden answers assume the probe setup (a list document and trending-score updates) and the
    lifecycle updates, which a read-only server cannot reproduce, so a few probes differ for that reason.
    """
    sys.path.insert(0, str(fixtures.parent / "e2e"))
    import probes as probe_defs  # minisolr's e2e/probes.py: probe definitions only, nothing runs on import

    gold = json.loads((fixtures / "probes_golden.json").read_text())
    ok = answered = total = 0
    reasons: Counter[str] = Counter()
    fails: list[str] = []
    for p in probe_defs.PROBES:
        g = gold.get(p["name"])
        if p.get("kind", "select") != "select" or g is None or "error" in g:
            continue
        total += 1
        pairs = [(k, str(x)) for k, v in p["params"].items() for x in (v if isinstance(v, list) else [v])]
        j = post(base, pairs)
        if reason := unanswered_reason(j):
            reasons[reason_group(reason)] += 1
            continue
        answered += 1
        r = probe_essence(p, j)
        bad = []
        if "numFound" in g and g["numFound"] != r.get("numFound"):
            bad.append(f"numFound {g['numFound']} != {r.get('numFound')}")
        if not p.get("count_only") and "keys" in g:
            gk, rk = g["keys"], r.get("keys", [])
            common = len(set(gk) & set(rk))
            tie_noise = g.get("numFound", 0) > len(gk)
            if set(gk) != set(rk) and gk and common / len(gk) < 0.8 and not tie_noise:
                bad.append(f"top-10 sets differ {common}/{len(gk)}")
        if "docs" in g and not p.get("count_only") and g["docs"] != r.get("docs"):
            bad.append("docs differ")
        if bad:
            fails.append(f"{p['name']}: " + "; ".join(bad))
        else:
            ok += 1
    print(f"select probes with golden: {total}; answerable: {answered}; match: {ok}/{answered}")
    for reason, n in reasons.most_common():
        print(f"  {n:4d}  {reason}")
    for f in fails if verbose else fails[:10]:
        print("  PROBE FAIL", f)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("base", help="core URL, e.g. http://127.0.0.1:8983/solr/openlibrary")
    ap.add_argument("fixtures", type=Path, help="minisolr fixtures directory (requests.json, golden.json)")
    ap.add_argument("--probes", action="store_true", help="also run the select-kind probes from ../e2e/probes.py")
    ap.add_argument("-v", "--verbose", action="store_true", help="list every miss and unanswered request")
    ap.add_argument("--subset-out", type=Path, help="write the params of answerable requests here (for tools/bench.py)")
    args = ap.parse_args()
    replay(args.base, args.fixtures, args.verbose, args.subset_out)
    if args.probes:
        probes(args.base, args.fixtures, args.verbose)


if __name__ == "__main__":
    main()
