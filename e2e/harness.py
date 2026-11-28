#!/usr/bin/env python3
"""End-to-end harness: runs the Open Library dev stack on minisolr (or real Solr) and checks it.

Needs an Open Library checkout: set OL_DIR (or run from inside it). Use `--help` for the steps.
"""

from __future__ import annotations

import argparse
import json
import os
import statistics
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import TYPE_CHECKING, Any

import probes

if TYPE_CHECKING:
    from collections.abc import Callable, Iterable

Params = list[tuple[str, str]]
Request = dict[str, Any]
SolrJson = dict[str, Any]
StepResult = list[tuple[str, bool]]

ROOT = Path(__file__).resolve().parents[1]
HERE = ROOT / "e2e"
FIX = ROOT / "fixtures"
STATE = ROOT / ".e2e"
REQUESTS = FIX / "requests.json"
GOLDEN = FIX / "golden.json"
DOCS = FIX / "docs.jsonl"
LAST_COMPARE = STATE / "last_compare.json"
MEASUREMENTS = STATE / "measurements.jsonl"
ENV_FILE = STATE / "env"
ENGINE_FILE = STATE / "engine"


def find_ol_dir() -> Path:
    for c in (os.environ.get("OL_DIR"), str(Path.cwd())):
        if c and (Path(c) / "compose.yaml").exists() and (Path(c) / "conf" / "solr").exists():
            return Path(c).resolve()
    raise SystemExit("Open Library checkout not found: set OL_DIR=/path/to/openlibrary (it needs compose.yaml and conf/solr)")


PROJECT = "minisolr-e2e"
SOLR_PORT = 9883
SOLR: str = f"http://localhost:{SOLR_PORT}/solr/openlibrary"
SERVICES = ["db", "memcached", "infobase", "web", "fast_web", "mockservices", "solr"]
ENV_LINES = {
    "COMPOSE_PROJECT_NAME": PROJECT,
    "SOLR_PORT": str(SOLR_PORT),
    "FAST_WEB_PORT": "8880",
    "COVERS_PORT": "7875",
    "MOCKSERVICES_PORT": "8825",
    "WEB_DEBUG_PORT": "3800",
    "FAST_WEB_DEBUG_PORT": "3801",
}
EXPECTED_DOCS = 446
EXPECTED_TYPES = {"work": 35, "edition": 68, "author": 34, "subject": 309}

URLS = [
    "/search.json?q=twain",
    "/search.json?q=mark+twain&fields=key,title,author_name,editions",
    "/search.json?q=title:alice",
    "/search.json?q=author_key:OL4A",
    "/search.json?q=web+design&sort=new",
    "/search.json?q=css&sort=old",
    "/search.json?q=web&sort=random",
    "/search.json?q=web&sort=rating",
    "/search.json?q=*:*&sort=editions",
    "/search.json?q=language:eng&facet=true&fields=key,title",
    "/search.json?q=subject:fiction",
    "/search.json?q=subject:%22lending+library%22",
    "/search.json?q=has_fulltext:true",
    "/search.json?q=ebook_access:public",
    "/search.json?q=first_publish_year:[1900+TO+2000]",
    "/search.json?q=publisher:penguin",
    "/search.json?q=isbn:9780140449136",
    "/search.json?q=lewis+carroll+alice",
    "/search.json?q=%22tom+sawyer%22",
    "/search.json?q=robinson+crusoe&limit=5&page=1",
    "/search.json?q=gulliver%27s+travels",
    "/search.json?q=title:%22green+eggs%22+AND+author:seuss",
    "/search.json?q=eggs&fields=*,editions",
    "/search.json?q=odyssey+homer",
    "/search.json?q=flatland",
    "/search.json?q=responsive+web+design&mode=everything",
    "/search.json?q=twain&mode=ebooks",
    "/search.json?q=twain&mode=printdisabled",
    "/search.json?q=twain&author_facet=OL18319A",
    "/search.json?q=*&language=eng&has_fulltext=true",
    "/search.json?q=sex&_spellcheck_count=3",
    "/search.json?q=twian&_spellcheck_count=3",
    "/search.json?q=alise+in+wonderland",
    "/search/authors.json?q=twain",
    "/search/authors.json?q=lewis",
    "/search/authors.json?q=carroll&limit=3",
    "/search/subjects.json?q=fiction",
    "/search/subjects.json?q=web",
    "/search/subjects.json?q=american",
    "/search/lists.json?q=a",
    "/search.json?q=*:*&facet=true&fields=key",
    "/search.json?q=edition_key:OL4M",
    "/search.json?q=ia:*",
    "/search.json?q=trending&sort=trending",
    "/works/OL45310W",
    "/works/OL53924W",
    "/works/OL20600W/Gulliver's_travels",
    "/books/OL4M",
    "/authors/OL4A",
    "/authors/OL4A/Kamala_Das",
    "/subjects/fiction",
    "/subjects/web_design",
    "/subjects/accessible_book",
    "/subjects/place:london",
    "/publishers/Penguin",
    "/search?q=twain",
    "/search?q=alice&mode=everything",
    "/search?q=css&sort=new",
    "/search?q=web&author_facet=x",
    "/search?q=mark+twain&has_fulltext=true",
    "/search?q=subject%3Afiction",
    "/search?q=title%3Aflatland",
    "/search/inside.json?q=alice",
    "/search?q=the",
    "/search?q=&mode=everything",
    "/search/authors?q=twain",
    "/search/subjects?q=fiction",
    "/trending/daily",
    "/",
    "/lists",
    "/browse",
    "/account/books",
    "/works/OL45310W/editions.json",
    "/authors/OL4A/works.json",
    "/subjects/fiction.json",
    "/query.json?type=/type/edition&limit=1",
    "/people/openlibrary/lists",
    "/books/OL4M/Enr%CC%B2e_sn%C4%93hita_Aru%E1%B9%87a",
    "/works/OL45310W/Robinson_Crusoe?edition=",
    "/search.json?q=twain&facet=true&fields=key,title,author_name,editions",
    "/search.json?q=alice&facet=true&language=eng",
    "/search.json?q=web&facet=true&subject_facet=Web+design",
    "/search.json?q=fiction&facet=true&has_fulltext=true&author_facet=OL18319A",
]

# Parameters that only matter to real Solr and the shim ignores.
DROP_PARAMS = {"timeAllowed"}


def say(*a: object) -> None:
    print(*a, flush=True)


def get_engine(default: str = "minisolr") -> str:
    try:
        return ENGINE_FILE.read_text().strip() or default
    except OSError:
        return default


def minisolr_compose_file() -> Path:
    """The override that provides the minisolr service; MINISOLR_COMPOSE_FILE selects another implementation."""
    return Path(os.environ.get("MINISOLR_COMPOSE_FILE") or ROOT / "compose.minisolr.yaml").resolve()


def minisolr_dir() -> str:
    """The image's build context: MINISOLR_DIR, else this checkout."""
    return str(Path(os.environ.get("MINISOLR_DIR") or ROOT).resolve())


def compose_files(engine: str) -> str:
    ol = find_ol_dir()
    files = [ol / "compose.yaml", ol / "compose.override.yaml", HERE / "compose.solr.yaml" if engine == "solr" else minisolr_compose_file()]
    return ":".join(str(f) for f in files)


def dc(
    *args: str,
    check: bool = True,
    capture: bool = False,
    engine: str | None = None,
    timeout: int = 600,
    input_text: str | None = None,
) -> subprocess.CompletedProcess[str]:
    """Run `docker compose <args>` in the Open Library checkout with this project's compose files and its own env file."""
    env = dict(os.environ)
    env["COMPOSE_FILE"] = compose_files(engine or get_engine())
    env["COMPOSE_PROJECT_NAME"] = PROJECT
    env["MINISOLR_DIR"] = minisolr_dir()
    env["HOSTNAME"] = ""
    return subprocess.run(
        ["docker", "compose", "--env-file", str(ENV_FILE), *args],
        cwd=find_ol_dir(),
        env=env,
        check=check,
        text=True,
        input=input_text,
        timeout=timeout,
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.STDOUT if capture else None,
    )


def write_env() -> None:
    STATE.mkdir(exist_ok=True)
    ENV_FILE.write_text("".join(f"{k}={v}\n" for k, v in ENV_LINES.items()))


def wait_healthy(timeout: int = 300) -> None:
    url = SOLR + "/admin/ping"
    end = time.time() + timeout
    last: Exception | None = None
    while time.time() < end:
        try:
            with urllib.request.urlopen(url, timeout=5) as r:
                if r.status == 200:
                    say("solr healthy")
                    return
        except Exception as e:
            last = e
        time.sleep(3)
    raise SystemExit(f"solr did not become healthy within {timeout}s: {last}")


def http_json(url: str, params: Params | dict[str, Any] | None = None, timeout: int = 60) -> tuple[SolrJson, float]:
    """POST (or GET) and return (json, elapsed_ms); errors become {"error": {"msg": ...}}."""
    t = time.time()
    data = urllib.parse.urlencode(params).encode() if params is not None else None
    try:
        with urllib.request.urlopen(urllib.request.Request(url, data=data), timeout=timeout) as resp:
            j = json.loads(resp.read())
    except urllib.error.HTTPError as e:
        raw = e.read()
        try:
            j = json.loads(raw)
            if "error" not in j:
                j = {"error": {"msg": f"HTTP {e.code}"}}
        except ValueError:
            j = {"error": {"msg": f"HTTP {e.code}: {raw[:200].decode(errors='replace')}"}}
    except Exception as e:
        j = {"error": {"msg": f"{type(e).__name__}: {e}"}}
    return j, (time.time() - t) * 1000


def err_msg(j: SolrJson) -> str:
    e = j.get("error")
    if isinstance(e, dict):
        return str(e.get("msg"))[:200]
    return str(e)[:200]


def load_requests() -> list[Request]:
    if not REQUESTS.exists():
        raise SystemExit(f"{REQUESTS} missing; run `traffic` against a solr stack first")
    return json.loads(REQUESTS.read_text())


def _dedupe(reqs: Iterable[Request]) -> list[Request]:
    return list({req_id(r): r for r in reqs}.values())


def select_requests(reqs: list[Request]) -> list[Request]:
    """Top-level /select requests with an ol.label, deduped."""
    return _dedupe(r for r in reqs if r["path"] == "/select" and "ol.label" in dict(r["params"]) and not any(k.startswith("{params") for k, _ in r["params"]))


def get_requests(reqs: list[Request]) -> list[Request]:
    return _dedupe(r for r in reqs if r["path"] == "/get")


def clean(params: Iterable[Iterable[str]]) -> Params:
    return [(k, v) for k, v in map(tuple, params) if k not in DROP_PARAMS]


def req_id(r: Request) -> str:
    return json.dumps(r["params"])


def label_q(r: Request) -> tuple[Any, str]:
    d = dict(r["params"])
    return d.get("ol.label"), (d.get("userWorkQuery") or d.get("q") or "")[:60]


def run_select(r: Request) -> tuple[SolrJson, float]:
    return http_json(SOLR + "/select", clean(r["params"]))


def run_get(r: Request) -> tuple[SolrJson, float]:
    return http_json(SOLR + "/get", clean(r["params"]))


def doc_keys(j: SolrJson) -> list[Any]:
    return [d.get("key") for d in j.get("response", {}).get("docs", [])]


def facets(j: SolrJson) -> dict[str, Any]:
    f = j.get("facet_counts", {}).get("facet_fields", {})
    return {k: [list(x) if isinstance(x, list) else x for x in v][:5] for k, v in f.items()}


def edition_keys(j: SolrJson) -> list[Any]:
    out: list[Any] = []
    for d in j.get("response", {}).get("docs", [])[:10]:
        eds = (d.get("editions") or {}).get("docs") if isinstance(d.get("editions"), dict) else None
        out.append(eds[0].get("key") if eds else None)
    return out


def summarize(j: SolrJson) -> dict[str, Any]:
    s = {"numFound": j.get("response", {}).get("numFound"), "keys": doc_keys(j)[:10], "facets": facets(j)}
    if any(isinstance(d.get("editions"), dict) for d in j.get("response", {}).get("docs", [])):
        s["edition_firsts"] = edition_keys(j)
    return s


def _unordered(v: Any) -> Any:
    # OL builds multi-valued fields such as `language` from Python sets, so their order changes per process
    return sorted(v, key=repr) if isinstance(v, list) else v


def summarize_get(j: SolrJson) -> dict[str, Any]:
    d = j.get("doc")
    return {"doc": {k: _unordered(v) for k, v in d.items()} if isinstance(d, dict) else d}


def cmd_up(args: argparse.Namespace) -> None:
    engine = args.engine
    write_env()
    ENGINE_FILE.write_text(engine)
    say(f"engine={engine} project={PROJECT} ol={find_ol_dir()}" + (f" override={minisolr_compose_file()} build={minisolr_dir()}" if engine == "minisolr" else ""))
    for attempt in (1, 2):
        r = dc("up", "-d", "--build", *SERVICES, check=False, engine=engine, timeout=1800)
        if r.returncode == 0:
            break
        if attempt == 1:
            say("compose up failed; retrying once (transient build errors happen)")
    else:
        raise SystemExit("compose up failed twice")
    wait_healthy()


def cmd_reindex(_args: argparse.Namespace) -> None:
    r = dc("run", "--rm", "-T", "home", "make", "reindex-solr", check=False, capture=True, timeout=1800)
    tail = (r.stdout or "").strip().splitlines()[-3:]
    for line in tail:
        say("  " + line)
    if r.returncode != 0:
        raise SystemExit(f"reindex failed (exit {r.returncode})")
    time.sleep(3)
    j, _ = http_json(SOLR + "/select", [("q", "*:*"), ("rows", "0"), ("facet", "true"), ("facet.field", "type"), ("json.nl", "arrarr")])
    if "error" in j:
        raise SystemExit("count query failed: " + err_msg(j))
    n = j["response"]["numFound"]
    types = dict(j["facet_counts"]["facet_fields"]["type"])
    say(f"docs: {n} (expected {EXPECTED_DOCS})")
    say(f"types: {types} (expected {EXPECTED_TYPES})")
    if n != EXPECTED_DOCS or types != EXPECTED_TYPES:
        say("WARNING: counts differ from expected")
        raise SystemExit(1)


def parse_solr_log(text: str) -> list[Request]:
    reqs: list[Request] = []
    seen: set[tuple[str, str]] = set()
    for line in text.splitlines():
        if "o.a.s.c.S.Request" not in line or " path=" not in line:
            continue
        path = line.split(" path=", 1)[1].split(" ", 1)[0]
        if path not in ("/select", "/get") or " params={" not in line:
            continue
        rest = line.split(" params={", 1)[1]
        if rest.startswith("{params("):
            continue
        body, _, tail = rest.rpartition("} ")
        if not body:
            continue
        hits: str | None = None
        status: str | None = None
        for tok in tail.split():
            if tok.startswith("hits="):
                hits = tok[5:]
            elif tok.startswith("status="):
                status = tok[7:]
        params = urllib.parse.parse_qsl(body, keep_blank_values=True)
        key = (path, json.dumps(params))
        if key in seen:
            continue
        seen.add(key)
        reqs.append({"path": path, "params": [list(p) for p in params], "hits": hits, "status": status})
    return reqs


def cmd_traffic(_args: argparse.Namespace) -> None:
    if get_engine() != "solr":
        say("note: traffic extraction needs the real Solr log; the current stack is not engine=solr")
    stdin = "\n".join(URLS) + "\n"
    script = 'while read u; do curl -sL -o /dev/null --max-time 120 "http://localhost:8080$u"; done'
    for svc in ("web", "fast_web"):  # legacy web.py and FastAPI each issue their own Solr requests
        say(f"replaying {len(URLS)} paths through {svc}")
        dc("exec", "-T", svc, "sh", "-c", script, input_text=stdin, timeout=3600)
    time.sleep(2)
    r = dc("logs", "--no-log-prefix", "solr", capture=True, timeout=300)
    reqs = parse_solr_log(r.stdout or "")
    selects = [x for x in reqs if x["path"] == "/select" and "ol.label" in dict(x["params"])]
    gets = [x for x in reqs if x["path"] == "/get"]
    keep = selects + gets
    FIX.mkdir(parents=True, exist_ok=True)
    REQUESTS.write_text(json.dumps(keep, indent=0))
    say(f"saved {len(keep)} requests ({len(selects)} select, {len(gets)} get) to {REQUESTS.relative_to(ROOT)}")
    if not selects:
        raise SystemExit("no labelled /select requests found in the solr log")


def cmd_golden(_args: argparse.Namespace) -> None:
    reqs = load_requests()
    sels, gets = select_requests(reqs), get_requests(reqs)
    gold = {"select": {}, "get": {}}
    bad = 0
    for r in sels:
        j, _ = run_select(r)
        if "error" in j:
            bad += 1
            say("solr error:", label_q(r), err_msg(j))
            continue
        gold["select"][req_id(r)] = summarize(j)
    for r in gets:
        j, _ = run_get(r)
        if "error" in j:
            bad += 1
            continue
        gold["get"][req_id(r)] = summarize_get(j)
    GOLDEN.write_text(json.dumps(gold, indent=1))
    say(f"golden: {len(gold['select'])} select, {len(gold['get'])} get, {bad} errors -> {GOLDEN.relative_to(ROOT)}")
    if bad:
        raise SystemExit(1)


def do_compare() -> bool:
    if not GOLDEN.exists():
        raise SystemExit(f"{GOLDEN} missing; run `golden` against a solr stack first")
    reqs = load_requests()
    gold = json.loads(GOLDEN.read_text())
    sels, gets = select_requests(reqs), get_requests(reqs)
    rows: list[dict[str, Any]] = []
    errors = 0
    nf_eq = ok_n = same_order = set_full = 0
    overlap = total = 0
    fac_eq = fac_n = 0
    ed_eq = ed_n = 0
    for r in sels:
        g = gold["select"].get(req_id(r))
        if g is None:
            continue
        j, ms = run_select(r)
        label, q = label_q(r)
        row = {"label": label, "q": q, "ms": round(ms, 1), "problems": []}
        if "error" in j:
            errors += 1
            row["error"] = err_msg(j)
            row["problems"].append("error")
            say(f"ERROR [{label}] {q!r}: {row['error']}")
            rows.append(row)
            continue
        ok_n += 1
        s = summarize(j)
        if s["numFound"] == g["numFound"]:
            nf_eq += 1
        else:
            row["problems"].append(f"numFound {s['numFound']} != {g['numFound']}")
        n = len(g["keys"])
        ov = len(set(s["keys"]) & set(g["keys"]))
        overlap += ov
        total += n
        if ov == n:
            set_full += 1
        else:
            row["problems"].append(f"top10 overlap {ov}/{n}")
        if s["keys"] == g["keys"]:
            same_order += 1
        if g["facets"]:
            fac_n += 1
            if s["facets"] == g["facets"]:
                fac_eq += 1
            else:
                row["problems"].append("facets differ")
        if "edition_firsts" in g:
            ed_n += 1
            if s.get("edition_firsts") == g["edition_firsts"]:
                ed_eq += 1
        rows.append(row)
    get_eq = get_n = 0
    for r in gets:
        g = gold["get"].get(req_id(r))
        if g is None:
            continue
        j, _ = run_get(r)
        get_n += 1
        if "error" in j:
            errors += 1
            say("ERROR /get", dict(r["params"]).get("id"), err_msg(j))
            continue
        if summarize_get(j) == summarize_get(g):
            get_eq += 1
        else:
            got, want = (summarize_get(j).get("doc") or {}), (summarize_get(g).get("doc") or {})
            diff = {k: {"solr": want.get(k), "minisolr": got.get(k)} for k in sorted(set(got) | set(want)) if got.get(k) != want.get(k)}
            rows.append(
                {
                    "label": "/get",
                    "q": str(dict(r["params"]).get("id")),
                    "problems": ["get differs in " + ", ".join(f"{k} (solr={v['solr']!r}, minisolr={v['minisolr']!r})" for k, v in diff.items())],
                }
            )
    pct = (overlap / total * 100) if total else 0.0
    passed = errors == 0 and ok_n > 0 and nf_eq == ok_n and pct >= 90
    summary = {
        "requests": len(sels),
        "errors": errors,
        "numFound_equal": f"{nf_eq}/{ok_n}",
        "top10_overlap_pct": round(pct, 2),
        "top10_set_equal": f"{set_full}/{ok_n}",
        "top10_same_order": f"{same_order}/{ok_n}",
        "facets_equal": f"{fac_eq}/{fac_n}",
        "editions_first_equal": f"{ed_eq}/{ed_n}",
        "get_equal": f"{get_eq}/{get_n}",
        "pass": passed,
    }
    for row in rows:
        if row["problems"] and "error" not in row["problems"]:
            say(f"MISMATCH [{row['label']}] {row['q']!r}: {'; '.join(row['problems'])}")
    LAST_COMPARE.write_text(json.dumps({"summary": summary, "rows": rows}, indent=1))
    for k, v in summary.items():
        say(f"  {k}: {v}")
    say(("COMPARE PASS" if passed else "COMPARE FAIL") + f" (numFound {nf_eq}/{ok_n}, overlap {pct:.1f}%, errors {errors})")
    return passed


def cmd_compare(_args: argparse.Namespace) -> None:
    if not do_compare():
        raise SystemExit(1)


def cmd_smoke(_args: argparse.Namespace) -> None:
    dc("cp", str(HERE / "smoke.py"), "fast_web:/tmp/smoke.py")
    r = dc("exec", "-T", "fast_web", "python", "/tmp/smoke.py", "http://localhost:8080", check=False, timeout=1800)
    if r.returncode:
        raise SystemExit(r.returncode)


def mem_of_solr() -> str | None:
    r = dc("ps", "-q", "solr", capture=True, check=False)
    cid = (r.stdout or "").strip().splitlines()[-1:] or [""]
    if not cid[0]:
        return None
    out = subprocess.run(["docker", "stats", "--no-stream", "--format", "{{.MemUsage}}", cid[0]], capture_output=True, text=True, timeout=60, check=False)
    return out.stdout.strip() or None


def do_measure(engine: str) -> dict[str, Any]:
    reqs = select_requests(load_requests())
    # Fair idle: restart the engine, let it settle, then read memory before any query.
    dc("restart", "solr", timeout=300)
    wait_healthy()
    time.sleep(45)
    mem_idle = mem_of_solr()
    lat = [run_select(r)[1] for _ in range(5) for r in reqs]
    lat.sort()
    mem_after = mem_of_solr()
    res = {
        "engine": engine,
        "mem_idle": mem_idle,
        "mem_after": mem_after,
        "latency": {
            "n": len(lat),
            "p50_ms": round(lat[len(lat) // 2], 1),
            "p95_ms": round(lat[int(len(lat) * 0.95)], 1),
            "mean_ms": round(statistics.mean(lat), 1),
        },
        "timestamp": time.strftime("%Y-%m-%dT%H:%M:%S"),
    }
    STATE.mkdir(exist_ok=True)
    with MEASUREMENTS.open("a") as f:
        f.write(json.dumps(res) + "\n")
    say(json.dumps(res))
    return res


def do_updater() -> bool:
    """Run the real solr-updater daemon, edit a work + create a list through the app, expect them searchable."""
    token = "updtoken" + time.strftime("%H%M%S")
    dc("up", "-d", "fast_web", "solr-updater", timeout=600)
    time.sleep(15)
    dc("cp", str(HERE / "updater_edit.sh"), "fast_web:/tmp/updater_edit.sh")
    dc("exec", "-T", "fast_web", "bash", "/tmp/updater_edit.sh", token, capture=True)
    base = f"http://localhost:{SOLR_PORT}/solr/openlibrary/select"
    t0 = time.time()
    found_work = found_list = False
    while time.time() - t0 < 180 and not (found_work and found_list):
        found_work = http_json(base, {"q": f"alternative_title:{token} AND type:work", "rows": 0})[0]["response"]["numFound"] == 1
        found_list = http_json(base, {"q": f"name:{token} AND type:list", "rows": 0})[0]["response"]["numFound"] == 1
        time.sleep(3)
    say(f"updater: work edit searchable={found_work}, new list searchable={found_list} after {time.time() - t0:.0f}s")
    dc("cp", str(HERE / "updater_list_delete.sh"), "fast_web:/tmp/updater_list_delete.sh")
    out = dc("exec", "-T", "fast_web", "bash", "/tmp/updater_list_delete.sh", "dl" + token, capture=True, timeout=300).stdout or ""
    lines = [x for x in out.splitlines() if x.startswith(("created", "after delete"))]
    say("updater list delete: " + " | ".join(lines))
    deleted_ok = bool(lines) and lines[-1].strip() == "after delete: 0" and "searchable after create: 1" in lines[0]
    return found_work and found_list and deleted_ok


def cmd_measure(args: argparse.Namespace) -> None:
    do_measure(args.engine or get_engine())


def cmd_export_docs(_args: argparse.Namespace) -> None:
    """Copy the shim's document store out of the container into fixtures/docs.jsonl (the no-Docker tests load it)."""
    # The store keeps recent updates in docs.wal; fold them into docs.jsonl (server stopped) before copying it out.
    dc("stop", "solr", timeout=120)
    dc("run", "--rm", "--no-deps", "--entrypoint", "/minisolr", "solr", "compact", "/conf", "/data", timeout=300)
    dc("cp", "solr:/data/docs.jsonl", str(DOCS))
    dc("start", "solr", timeout=120)
    say(f"wrote {DOCS} ({sum(1 for _ in DOCS.open())} docs)")


def cmd_restart_solr(_args: argparse.Namespace) -> None:
    dc("restart", "solr", timeout=300)
    wait_healthy()


def cmd_down(_args: argparse.Namespace) -> None:
    dc("down", "-v", "--remove-orphans", check=False, timeout=600)
    r = subprocess.run(
        ["docker", "ps", "-a", "--filter", f"label=com.docker.compose.project={PROJECT}", "--format", "{{.Names}}"],
        capture_output=True,
        text=True,
        timeout=60,
        check=False,
    )
    left = r.stdout.split()
    if left:
        raise SystemExit(f"containers still present: {left}")
    ENV_FILE.unlink(missing_ok=True)
    ENGINE_FILE.unlink(missing_ok=True)
    say(f"no {PROJECT} containers remain")


def step(name: str, fn: Callable[..., Any], results: StepResult, *a: Any) -> bool:
    say(f"\n=== {name} ===")
    try:
        r = fn(*a)
        ok = r is not False
    except SystemExit as e:
        ok = e.code in (0, None)
    except Exception as e:
        say(f"{name} raised {type(e).__name__}: {e}")
        ok = False
    results.append((name, ok))
    return ok


def cmd_all_minisolr(args: argparse.Namespace) -> None:
    ns = argparse.Namespace(engine="minisolr")
    res: StepResult = []
    if step("up minisolr", cmd_up, res, ns):
        step("reindex", cmd_reindex, res, ns)
        step("probe-setup", probes.setup, res, dc)
        if args.export_docs:
            step("export docs", cmd_export_docs, res, ns)
        step("smoke", cmd_smoke, res, ns)
        step("compare", do_compare, res)
        step("restart-solr", cmd_restart_solr, res, ns)
        step("compare (persistence)", do_compare, res)
        step("measure", do_measure, res, "minisolr")
        step("probes vs real Solr", probes.compare, res)
        step("solr-updater daemon", do_updater, res)
    say("\n=== checklist ===")
    for name, ok in res:
        say(f"{'PASS' if ok else 'FAIL':5} {name}")
    if args.down:
        cmd_down(ns)
    if not all(ok for _, ok in res):
        raise SystemExit(1)


def cmd_all_solr(_args: argparse.Namespace) -> None:
    ns = argparse.Namespace(engine="solr")
    cmd_up(ns)
    cmd_reindex(ns)
    probes.setup(dc)
    cmd_traffic(ns)
    cmd_golden(ns)
    do_measure("solr")
    probes.golden()
    cmd_down(ns)


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(dest="cmd", required=True)

    def add(name: str, fn: Callable[[argparse.Namespace], object], help_text: str, *, engine: bool = False) -> argparse.ArgumentParser:
        sp = sub.add_parser(name, help=help_text, description=help_text)
        if engine:
            sp.add_argument("--engine", choices=["solr", "minisolr"], required=name == "up", help="which search engine the stack uses")
        sp.set_defaults(fn=fn)
        return sp

    add("up", cmd_up, "start the stack with the chosen engine and wait until healthy", engine=True)
    add("reindex", cmd_reindex, "run make reindex-solr and print doc/type counts")
    add("traffic", cmd_traffic, "replay site paths and capture Solr requests into fixtures/requests.json")
    add("golden", cmd_golden, "replay fixtures against real Solr and store fixtures/golden.json")
    add("compare", cmd_compare, "replay fixtures against the running stack and compare to golden")
    add("smoke", cmd_smoke, "run smoke.py inside the web container")
    add("measure", cmd_measure, "memory + latency burst; appends to fixtures/measurements.jsonl", engine=True)
    add("probe-setup", lambda _: probes.setup(dc), "create a list + trending state for the feature probes")
    add("probe-golden", lambda _: probes.golden(), "run feature probes against real Solr -> fixtures/probes_golden.json")
    add("probe-compare", lambda _: sys.exit(0 if probes.compare() else 1), "run feature probes and compare to probes_golden.json")
    add("updater", lambda _: sys.exit(0 if do_updater() else 1), "run the solr-updater daemon and verify an edited work and a new list become searchable")
    add("restart-solr", cmd_restart_solr, "restart the solr service and wait until healthy")
    add("down", cmd_down, "tear down the stack and remove .env")
    add("export-docs", cmd_export_docs, "copy the running shim's documents to fixtures/docs.jsonl")
    sp = add("all-minisolr", cmd_all_minisolr, "up minisolr, reindex, smoke, compare, restart, compare, measure, probes, updater")
    sp.add_argument("--down", action="store_true", help="tear down afterwards")
    sp.add_argument("--export-docs", action="store_true", help="also refresh fixtures/docs.jsonl (state after probe-setup)")
    add("all-solr", cmd_all_solr, "up solr, reindex, traffic, golden, measure, down")

    args = p.parse_args()
    args.fn(args)


if __name__ == "__main__":
    sys.exit(main())
