"""Feature probes: build identical state on any engine, run targeted requests, compare to real Solr's answers.

Used by harness.py (`probe-golden` against real Solr, `probe-compare` against minisolr).
"""

from __future__ import annotations

import json
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from collections.abc import Callable

SolrJson = dict[str, Any]

ROOT = Path(__file__).resolve().parents[1]
GOLDEN = ROOT / "fixtures" / "probes_golden.json"
LAST = ROOT / ".e2e" / "last_probes.json"
BASE: str = "http://localhost:9883/solr/openlibrary"

QF = "alternative_title^40 author_name^40 series_name^5 chapter subject place person time publisher contributor"
ED = {"defType": "edismax", "qf": QF, "q.op": "AND"}

NEW_DOC = {
    "key": "/works/OL999999W",
    "type": "work",
    "title": "Zebra crossing handbook",
    "alternative_title": ["Zebra crossing handbook"],
    "author_name": ["Zed Tester"],
    "author_key": ["OL999999A"],
    "edition_count": 1,
    "ebook_access": "public",
    "has_fulltext": True,
    "language": ["eng"],
    "subject": ["Road safety"],
    "readinglog_count": 0,
}

# Each probe: name, kind (select|export|json|get|update|list), params
PROBES: list[dict[str, Any]] = [
    # --- text matching / parser ---
    {"name": "edismax single", "params": {**ED, "q": "twain", "fq": "type:work", "fl": "key"}},
    {"name": "edismax two terms AND", "params": {**ED, "q": "mark twain", "fq": "type:work", "fl": "key"}},
    {"name": "edismax mm=1 OR", "params": {"defType": "edismax", "qf": QF, "q": "twain alice flatland", "mm": "1", "fq": "type:work", "fl": "key"}},
    {"name": "edismax mm=2", "params": {"defType": "edismax", "qf": QF, "q": "twain tom sawyer", "mm": "2", "fq": "type:work", "fl": "key"}},
    {"name": "edismax mm=100%", "params": {"defType": "edismax", "qf": QF, "q": "mark twain tom", "mm": "100%", "fq": "type:work", "fl": "key"}},
    {
        "name": "edismax pf+pf2+boost",
        "params": {
            **ED,
            "q": "tom sawyer",
            "pf": "alternative_title^50",
            "pf2": "alternative_title^20",
            "boost": "sum(mul(20,log(sum(3,edition_count))),1)",
            "fq": "type:work",
            "fl": "key",
        },
    },
    {
        "name": "edismax bf",
        "params": {"defType": "edismax", "qf": "name alternate_names", "q": "twain", "bf": "min(work_count,20)", "fq": "type:author", "fl": "key"},
    },
    {"name": "edismax stopword-only", "params": {**ED, "q": "the", "fq": "type:work", "fl": "key"}},
    {"name": "edismax possessive/stem", "params": {**ED, "q": "gullivers travel", "fq": "type:work", "fl": "key"}},
    {"name": "edismax accents", "params": {**ED, "q": "enre snehita aruna", "fq": "type:work", "fl": "key"}},
    {"name": "edismax wdgf", "params": {**ED, "q": "html5", "fq": "type:work", "fl": "key"}},
    {"name": "edismax unknown field text", "params": {**ED, "q": "foo:bar", "fq": "type:work", "fl": "key"}},
    {"name": "lucene field:term", "params": {"q": "title:alice", "fl": "key"}},
    {"name": "lucene phrase", "params": {"q": 'alternative_title:"tom sawyer"', "fl": "key"}},
    {"name": "lucene phrase slop", "params": {"q": 'alternative_title:"mark wisdom"~3', "fl": "key"}},
    {"name": "lucene wildcard", "params": {"q": "alternative_title:fl*", "fl": "key"}},
    {"name": "lucene fuzzy", "params": {"q": "alternative_title:twian~", "fl": "key"}},
    {"name": "lucene boolean", "params": {"q": "alternative_title:(alice OR flatland) -subject:fiction AND type:work", "fl": "key"}},
    {"name": "lucene +/-", "params": {"q": "+type:work -ebook_access:no_ebook", "fl": "key"}},
    {"name": "range numeric incl", "params": {"q": "first_publish_year:[1900 TO 2000]", "fq": "type:work", "fl": "key"}},
    {"name": "range numeric excl", "params": {"q": "first_publish_year:{1900 TO 2000}", "fq": "type:work", "fl": "key"}},
    {"name": "range open", "params": {"q": "edition_count:[5 TO *]", "fq": "type:work", "fl": "key"}},
    {"name": "range enum", "params": {"q": "ebook_access:[printdisabled TO *]", "fq": "type:work", "fl": "key"}},
    {"name": "bool field", "params": {"q": "has_fulltext:true", "fq": "type:work", "fl": "key"}},
    {"name": "exists wildcard", "params": {"q": "ia:*", "fq": "type:work", "fl": "key"}},
    {"name": "terms fq", "params": {"q": "*:*", "fq": "{!terms f=key v='/works/OL53924W,/works/OL45310W,/works/OL20600W'}", "fl": "key"}},
    {"name": "terms author_key", "params": {"q": "*:*", "fq": ["type:work", "{!terms f=author_key v=OL18319A,OL22098A}"], "fl": "key"}},
    {"name": "local lucene tag", "params": {"q": "*:*", "fq": "{!tag=lang}language:eng", "fl": "key"}},
    # --- nested editions ---
    {
        "name": "parent join + subquery",
        "params": {
            "q": '+({!edismax q.op=AND qf="alternative_title^40 author_name^40" v=$uw}) '
            '+(_query_:"{!parent which=type:work v=$full filters=$editions.fq}" OR edition_count:0)',
            "uw": "twain",
            "full": '({!edismax v=$ufq qf="title publisher ia key"})',
            "ufq": "+language:eng",
            "editions.fq": "type:edition",
            "fq": "type:work",
            "fl": "key,editions:[subquery]",
            "editions.q": "+({!terms f=_root_ v=$row.key}) language:eng^40 cover_i:*^2",
            "editions.fq2": "x",
            "editions.rows": "2",
            "editions.fl": "key",
            "rows": "5",
        },
    },
    {"name": "child transformer", "params": {"q": 'key:"/works/OL45310W"', "fl": "key,[child]"}},
    # --- facets ---
    {
        "name": "facet field limit/sort",
        "params": {
            "q": "type:work",
            "rows": "0",
            "facet": "true",
            "facet.field": "subject_facet",
            "facet.limit": "10",
            "facet.mincount": "1",
            "json.nl": "arrarr",
        },
    },
    {
        "name": "facet index sort offset",
        "params": {
            "q": "type:work",
            "rows": "0",
            "facet": "true",
            "facet.field": "subject_facet",
            "facet.sort": "index",
            "facet.limit": "5",
            "facet.offset": "3",
            "facet.mincount": "1",
            "json.nl": "arrarr",
        },
    },
    {
        "name": "facet multi + per-field",
        "params": {
            "q": "type:work",
            "rows": "0",
            "facet": "true",
            "facet.field": ["language", "has_fulltext", "author_facet"],
            "f.author_facet.facet.limit": "3",
            "facet.mincount": "1",
            "json.nl": "arrarr",
        },
    },
    {"name": "facet flat nl", "params": {"q": "type:work", "rows": "0", "facet": "true", "facet.field": "language", "facet.mincount": "1"}},
    {"name": "facet zero counts", "params": {"q": "language:mal", "rows": "0", "facet": "true", "facet.field": "language", "json.nl": "arrarr"}},
    {
        "name": "facet prefix",
        "params": {
            "q": "type:work",
            "rows": "0",
            "facet": "true",
            "facet.field": "subject_facet",
            "facet.prefix": "Web",
            "facet.mincount": "1",
            "json.nl": "arrarr",
        },
    },
    {"name": "facet query", "params": {"q": "type:work", "rows": "0", "facet": "true", "facet.query": ["edition_count:[2 TO *]", "has_fulltext:true"]}},
    # --- sort ---
    {"name": "sort number desc", "params": {"q": "type:work", "sort": "edition_count desc, key asc", "fl": "key", "rows": "10"}},
    {"name": "sort year def asc", "params": {"q": "type:work", "sort": "def(first_publish_year, 9999) asc, key asc", "fl": "key", "rows": "10"}},
    {"name": "sort string key", "params": {"q": "type:work", "sort": "key desc", "fl": "key", "rows": "10"}},
    {"name": "sort ratings_sortable", "params": {"q": "type:author", "sort": "ratings_sortable desc, key asc", "fl": "key", "rows": "10"}},
    {"name": "sort title_sort (ICU)", "params": {"q": "type:work", "sort": "title_sort asc", "fl": "key", "rows": "10"}},
    {"name": "sort random", "params": {"q": "type:work", "sort": "random_7 asc", "fl": "key", "rows": "10"}, "count_only": True},
    {"name": "sort function trending", "params": {"q": "type:work", "sort": "def(trending_z_score, 0) desc, key asc", "fl": "key", "rows": "5"}},
    {"name": "paging start/rows", "params": {"q": "type:work", "sort": "key asc", "fl": "key", "start": "5", "rows": "5"}},
    {"name": "cursorMark", "params": {"q": "type:author", "sort": "key asc", "cursorMark": "*", "rows": "5", "fl": "key"}, "cursor": True},
    # --- highlighting / spellcheck ---
    {
        "name": "highlight",
        "params": {
            **ED,
            "q": "sawyer",
            "fq": "type:work",
            "fl": "key",
            "hl": "true",
            "hl.fl": "title,subject,chapter",
            "hl.q": "sawyer",
            "hl.snippets": "10",
            "hl.fragsize": "0",
        },
    },
    {
        "name": "highlight subject",
        "params": {
            **ED,
            "q": "fiction",
            "fq": "type:work",
            "fl": "key",
            "hl": "true",
            "hl.fl": "subject,chapter",
            "hl.q": "fiction",
            "hl.snippets": "10",
            "hl.fragsize": "0",
        },
    },
    {"name": "spellcheck typo", "params": {**ED, "q": "twian", "fq": "type:work", "rows": "0", "spellcheck": "true", "spellcheck.count": "3"}},
    {"name": "spellcheck ok term", "params": {**ED, "q": "twain", "fq": "type:work", "rows": "0", "spellcheck": "true", "spellcheck.count": "3"}},
    # --- other handlers ---
    {"name": "export", "kind": "export", "params": {"q": "type:work", "fl": "key,edition_count", "sort": "key asc"}},
    {"name": "get single", "kind": "get", "params": {"id": "/works/OL45310W", "fl": "key,title,edition_count"}},
    {"name": "get ids", "kind": "get", "params": {"ids": "/works/OL45310W,/works/OL53924W", "fl": "key,title"}},
    {
        "name": "json request + json facet",
        "kind": "json",
        "body": {
            "params": {"json.nl": "arrarr", "q": "author_key:OL18319A", "fq": "type:work", "fl": "title, subtitle", "sort": "edition_count desc"},
            "facet": {
                "want_to_read_count": "sum(want_to_read_count)",
                "readinglog_count": "sum(readinglog_count)",
                "subject_facet": {"type": "terms", "field": "subject_facet"},
            },
        },
    },
    # --- lists (created by setup) ---
    {
        "name": "list search",
        "params": {
            "defType": "edismax",
            "qf": "subject place person time name^10",
            "q": "twain",
            "fq": ["type:list OR list_type:*", "seed_count:[2 TO *] OR list_type:series"],
            "fl": "key,name,seed_count",
            "q.op": "AND",
        },
    },
    {"name": "list by seed", "params": {"q": "type:list", "fl": "key,name,seed_count"}},
    # --- trending state (set by setup via atomic updates) ---
    {
        "name": "trending export",
        "kind": "export",
        "params": {"q": "trending_score_hourly_sum:[1 TO *] OR trending_score_daily_0:[1 TO *]", "fl": "key,trending_score_hourly_sum", "sort": "key asc"},
    },
    {
        "name": "carousel trending",
        "params": {"q": "trending_score_hourly_sum:[1 TO *]", "fq": "type:work", "sort": "def(trending_z_score, 0) desc", "fl": "key", "rows": "6"},
    },
    # --- update lifecycle (sequence; ops run in order) ---
    {"name": "lifecycle: before add", "params": {"q": "alternative_title:zebra", "fl": "key"}},
    {"name": "lifecycle: add", "kind": "update", "body": {"add": {"doc": NEW_DOC}, "commit": {}}},
    {"name": "lifecycle: after add", "params": {"q": "alternative_title:zebra", "fl": "key,title,edition_count"}},
    {
        "name": "lifecycle: atomic inc/set",
        "kind": "update",
        "body": [{"key": "/works/OL999999W", "edition_count": {"inc": 4}, "title": {"set": "Zebra crossing handbook 2nd ed"}}],
        "commit": True,
    },
    {"name": "lifecycle: after atomic", "params": {"q": 'key:"/works/OL999999W"', "fl": "key,title,edition_count"}},
    {"name": "lifecycle: delete by id", "kind": "update", "body": {"delete": ["/works/OL999999W"], "commit": {}}},
    {"name": "lifecycle: after delete", "params": {"q": "alternative_title:zebra", "fl": "key"}},
    {"name": "lifecycle: delete by query", "kind": "update", "body": {"delete": {"query": 'key:"/works/NOPE"'}, "commit": {}}},
    # --- a field the schema does not define: real Solr answers HTTP 400 (error msg and code are compared) ---
    {"name": "undefined field: q", "expect_error": True, "params": {"q": "nosuch:foo", "fl": "key"}},
    {"name": "undefined field: OR clause", "expect_error": True, "params": {"q": "title:twain OR nosuch:foo", "fl": "key"}},
    {"name": "undefined field: AND clause", "expect_error": True, "params": {"q": "title:twain AND nosuch:foo", "fl": "key"}},
    {"name": "undefined field: fq", "expect_error": True, "params": {"q": "*:*", "fq": "nosuch:foo", "fl": "key"}},
    {"name": "undefined field: wildcard", "expect_error": True, "params": {"q": "nosuch:fo*", "fl": "key"}},
    {"name": "undefined field: range", "expect_error": True, "params": {"q": "nosuch:[1 TO 2]", "fl": "key"}},
    {"name": "undefined field: df", "expect_error": True, "params": {"q": "foo", "df": "nosuch", "fl": "key"}},
    {"name": "undefined field: {!terms}", "expect_error": True, "params": {"q": "{!terms f=nosuch}a,b", "fl": "key"}},
    {"name": "undefined field: {!field}", "expect_error": True, "params": {"q": "{!field f=nosuch}a", "fl": "key"}},
    {"name": "undefined field: {!prefix}", "expect_error": True, "params": {"q": "{!prefix f=nosuch}a", "fl": "key"}},
    {"name": "undefined field: sort", "expect_error": True, "params": {"q": "*:*", "sort": "nosuch desc", "fl": "key"}},
    {"name": "undefined field: facet.field", "expect_error": True, "params": {"q": "*:*", "facet": "true", "facet.field": "nosuch", "fl": "key"}},
    {"name": "undefined field: hl.fl", "expect_error": True, "params": {"q": "title:twain", "hl": "true", "hl.fl": "nosuch", "fl": "key"}},
    {"name": "undefined field: edismax qf", "expect_error": True, "params": {**ED, "qf": "title nosuch", "q": "twain", "fl": "key"}},
    {"name": "undefined field: edismax prefix is text", "params": {**ED, "q": "nosuch:foo", "fl": "key"}},
    {"name": "undefined field: delete by query", "kind": "update", "expect_error": True, "body": {"delete": {"query": "nosuch:foo"}, "commit": {}}},
]


def _call(path: str, params: dict[str, Any] | None = None, body: Any = None, raw: bytes | None = None, method: str = "GET") -> SolrJson:
    url = BASE + path
    data: bytes | None = None
    headers: dict[str, str] = {}
    if params is not None:
        pairs = [(k, str(x)) for k, v in params.items() for x in (v if isinstance(v, list) else [v])]
        data = urllib.parse.urlencode(pairs).encode()
        if method == "GET":
            url += "?" + data.decode()
            data = None
    if body is not None:
        data = json.dumps(body).encode() if raw is None else raw
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=data, headers=headers, method="POST" if data is not None else "GET")
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return json.loads(r.read() or b"{}")
    except urllib.error.HTTPError as e:
        try:
            return json.loads(e.read())
        except Exception:
            return {"error": {"msg": f"HTTP {e.code}"}}
    except Exception as e:
        return {"error": {"msg": f"{type(e).__name__}: {e}"}}


def setup(dc: Callable[..., Any]) -> None:
    """Identical state on either engine: a list, trending scores (atomic updates)."""
    script = (
        'curl -s -c /tmp/c.txt -X POST http://localhost:8080/account/login.json -H "Content-Type: application/json" '
        '-d \'{"username":"openlibrary","password":"openlibrary"}\' >/dev/null; '
        "curl -s -b /tmp/c.txt -X POST http://localhost:8080/people/openlibrary/lists.json -H 'Content-Type: application/json' "
        '-d \'{"name":"Twain and Carroll favorites","description":"probe list",'
        '"seeds":[{"key":"/works/OL53924W"},{"key":"/works/OL15298516W"},{"key":"/authors/OL18319A"}]}\''
    )
    dc("exec", "-T", "fast_web", "sh", "-c", script, capture=True, check=False)
    time.sleep(1)
    dc(
        "run",
        "--rm",
        "-T",
        "home",
        "python",
        "openlibrary/solr/update.py",
        "--ol-url",
        "http://web:8080/",
        "--ol-config",
        "conf/openlibrary.yml",
        "--solr-next",
        "/people/openlibrary/lists/OL1L",
        capture=True,
        check=False,
    )
    ups = [
        {"key": "/works/OL53924W", "trending_score_hourly_sum": {"set": 5}, "trending_score_hourly_4": {"set": 5}, "trending_z_score": {"set": 2.5}},
        {"key": "/works/OL45310W", "trending_score_hourly_sum": {"set": 3}, "trending_score_hourly_4": {"set": 3}, "trending_z_score": {"set": 1.0}},
    ]
    _call("/update?update.partial.requireInPlace=true&commit=true", body=ups, method="POST")


def run_one(p: dict[str, Any]) -> SolrJson:
    kind = p.get("kind", "select")
    if kind == "update":
        body = p["body"]
        path = "/update?commit=true" if p.get("commit") else "/update"
        return _call(path, body=body, method="POST")
    if kind == "export":
        return _call("/export", params=p["params"], method="POST")
    if kind == "get":
        return _call("/get", params=p["params"])
    if kind == "json":
        return _call("/query", body=p["body"], method="POST")
    return _call("/select", params=p["params"], method="POST")


def norm(p: dict[str, Any], j: SolrJson) -> dict[str, Any]:
    """The comparable essence of a response."""
    if "error" in j:
        return {"error": str(j["error"].get("msg"))[:160], "code": j["error"].get("code")}
    kind = p.get("kind", "select")
    out: dict[str, Any] = {}
    if kind == "update":
        return {"status": j.get("responseHeader", {}).get("status")}
    if kind == "get":
        out["doc"] = j.get("doc")
        if "response" in j:
            out["docs"] = j["response"]["docs"]
        return out
    resp = j.get("response", {})
    if resp:
        out["numFound"] = resp.get("numFound")
        docs = resp.get("docs", [])
        out["keys"] = [d.get("key") for d in docs][:10]
        fl = p.get("params", {}).get("fl", "")
        if kind == "export" or ("," in fl and "editions" not in fl and "[child]" not in fl):
            out["docs"] = [{k: v for k, v in d.items() if k != "score"} for d in docs[:10]]
        if docs and isinstance(docs[0].get("editions"), dict):
            out["editions"] = [[e.get("key") for e in d["editions"]["docs"]] for d in docs]
        if docs and isinstance(docs[0].get("editions"), list):
            out["children"] = [sorted(e.get("key") for e in d.get("editions", [])) for d in docs]
    if "facet_counts" in j:
        fc = j["facet_counts"]
        out["facet_fields"] = fc.get("facet_fields")
        out["facet_queries"] = fc.get("facet_queries")
    if "facets" in j:
        out["json_facets"] = j["facets"]
    if "highlighting" in j:
        out["highlighting"] = j["highlighting"]
    if "spellcheck" in j:
        sug = j["spellcheck"].get("suggestions", [])
        words = [(sug[i], sug[i + 1].get("suggestion", [])[:3]) for i in range(0, len(sug) - 1, 2) if isinstance(sug[i + 1], dict)]
        out["spellcheck"] = words
    if "nextCursorMark" in j:
        out["cursor_advances"] = j["nextCursorMark"] != "*"
    return out


def run_all() -> dict[str, dict[str, Any]]:
    results: dict[str, dict[str, Any]] = {}
    for p in PROBES:
        j = run_one(p)
        results[p["name"]] = norm(p, j)
    return results


def golden() -> None:
    res = run_all()
    errs = {k: v["error"] for k, v in res.items() if "error" in v}
    GOLDEN.write_text(json.dumps(res, indent=1, sort_keys=True))
    print(f"probes golden: {len(res)} probes, {len(errs)} errors from real Solr -> {GOLDEN}")
    for k, e in errs.items():
        print(f"  solr error [{k}]: {e}")


def _as_set(x: Any) -> set[Any]:
    return set(x) if isinstance(x, list) else set()


def compare() -> bool:
    gold = json.loads(GOLDEN.read_text())
    res = run_all()
    probes = {p["name"]: p for p in PROBES}
    fails: list[str] = []
    notes: list[str] = []
    ok = 0
    for name, g in gold.items():
        r = res.get(name, {"error": "missing"})
        p = probes[name]
        if "error" in g:
            if p.get("expect_error"):  # the rejection itself is what real Solr does
                if (r.get("error"), r.get("code")) == (g["error"], g.get("code")):
                    ok += 1
                else:
                    fails.append(f"{name}: expected HTTP {g.get('code')} {g['error']!r}, got {r}")
            continue  # otherwise real Solr rejected it; nothing to match
        if "error" in r:
            fails.append(f"{name}: minisolr error: {r['error']}")
            continue
        bad = []
        if "numFound" in g and g["numFound"] != r.get("numFound"):
            bad.append(f"numFound {g['numFound']} != {r.get('numFound')}")
        if not p.get("count_only") and "keys" in g:
            gk, rk = g["keys"], r.get("keys", [])
            if set(gk) != set(rk):
                tie_noise = g.get("numFound", 0) > len(gk)  # which of many equal-score hits fill the top 10 is arbitrary
                (bad if len(gk) and len(_as_set(gk) & _as_set(rk)) / len(gk) < 0.8 and not tie_noise else notes).append(
                    f"{name}: top-10 sets differ {len(_as_set(gk) & _as_set(rk))}/{len(gk)}"
                )
            elif gk != rk:
                notes.append(f"{name}: same top-10 set, different order")
        for k in ("facet_fields", "facet_queries", "json_facets", "doc", "docs", "children", "cursor_advances"):
            if k in g and g[k] != r.get(k):
                if k == "docs" and p.get("count_only"):
                    continue
                bad.append(f"{k} differ")
        if "editions" in g and g["editions"] != r.get("editions"):
            notes.append(f"{name}: edition choice/order differs")
        if "highlighting" in g:
            if set(g["highlighting"]) != set(r.get("highlighting", {})):
                bad.append(f"highlight docs {sorted(g['highlighting'])} != {sorted(r.get('highlighting', {}))}")
            elif g["highlighting"] != r.get("highlighting"):
                notes.append(f"{name}: highlight snippets differ in text")
        if "spellcheck" in g:
            gw = [w for w, _ in g["spellcheck"]]
            rw = [w for w, _ in r.get("spellcheck", [])]
            if gw != rw:
                bad.append(f"spellcheck terms {gw} != {rw}")
            elif g["spellcheck"] != r.get("spellcheck"):
                notes.append(f"{name}: spellcheck suggestions differ {g['spellcheck']} vs {r.get('spellcheck')}")
        if bad:
            fails.append(f"{name}: " + "; ".join(bad))
        else:
            ok += 1
    total = len([1 for n, g in gold.items() if "error" not in g or probes[n].get("expect_error")])
    print(f"probes: {ok}/{total} match real Solr (hard checks); {len(fails)} failing, {len(notes)} soft differences")
    for f in fails:
        print("  FAIL", f)
    for n in notes:
        print("  note", n)
    LAST.parent.mkdir(exist_ok=True)
    LAST.write_text(json.dumps({"fails": fails, "notes": notes, "ok": ok, "total": total}, indent=1))
    return not fails
