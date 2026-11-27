# minisolr-rs

A Rust port of [minisolr](../minisolr), the small Solr stand-in for Open Library development. It began as a prototype of `/select` to answer two questions: how much RAM does a Rust + Tantivy version really use, and can the Solr analyzer chains be reproduced token for token. It is now being ported step by step; [PORTING.md](PORTING.md) tracks the steps.

## Run

```bash
mkdir -p /tmp/ms && cp ../minisolr/fixtures/docs.jsonl /tmp/ms/
cargo run --release -- serve ../minisolr/conf /tmp/ms 127.0.0.1:8983
curl 'localhost:8983/solr/openlibrary/select?q=author_name:carroll&fq=type:work&fl=key'
curl -X POST 'localhost:8983/solr/openlibrary/update?commit=true' -H 'Content-Type: application/json' \
  -d '{"add": {"doc": {"key": "/works/OL1W", "type": "work", "title": "x"}}, "commit": {}}'
cargo test --release
```

The first argument is a Solr conf directory with `managed-schema.xml` (and optionally `enumsConfig.xml`). The second is the data directory (created if missing): the store is loaded from `<dir>/docs.jsonl` and saved there after every update request that changes something, in the same format as Python minisolr, so either server can open the other's directory. Given a `.jsonl` file instead, the server loads it and keeps updates in memory only.

## Tools

All in `tools/`, Python. `replay.py` and `bench.py` use the standard library only and start nothing; `compare_python.py` needs Python minisolr's environment.

```bash
# correctness against real Solr's captured answers (the main progress meter)
python3 tools/replay.py http://127.0.0.1:8983/solr/openlibrary ../minisolr/fixtures [--probes] [-v] [--subset-out answered.json]
# same answers as Python minisolr? (runs Python minisolr in-process; --neutral turns its boosts off)
uv run --project ../minisolr python tools/compare_python.py http://127.0.0.1:8983/solr/openlibrary ../minisolr [--neutral]
# reindex an empty server through /update (works with nested editions, repeated `add` keys), then compare two servers
python3 tools/roundtrip.py load http://127.0.0.1:8984/solr/openlibrary ../minisolr/fixtures/docs.jsonl
python3 tools/roundtrip.py compare http://127.0.0.1:8983/solr/openlibrary http://127.0.0.1:8984/solr/openlibrary ../minisolr/fixtures
# memory and latency over the answerable requests
python3 tools/bench.py http://127.0.0.1:8983/solr/openlibrary <pid> answered.json 1300
# analyzer parity with Python minisolr, over every text field in the data
uv run --project ../minisolr python tools/dump_tokens.py ../minisolr/conf ../minisolr/fixtures/docs.jsonl > tokens.jsonl
cargo run --release -- parity ../minisolr/conf tokens.jsonl
```

## Schema

`src/schema.rs` reads `managed-schema.xml` and `enumsConfig.xml` (with `roxmltree`): field types (kind from the class, as in Python's `schema.py`), fields with Python's attribute defaults, dynamic fields (longest pattern first), copyFields, the uniqueKey and enum values. Every searchable static field is indexed, plus each dynamic field that occurs in the data; copyFields are applied at index time and when documents are returned. A text field type gets its analyzer from its index analyzer: `text_en_splitting`, `text_international` and `text_general`-shaped chains (and `text_gen_sort`, which has the same chain) are implemented; any other text type (`text_en`, the language types, `text_ws`, ...) falls back to whitespace + lowercase with a warning on stderr when a document uses it. None of the fields in the dev data need the fallback. Nested child documents are separate rows sharing `_root_`.

## Store, updates and concurrency

The store (`src/store.rs`) is the source of truth, as in Python: documents in insertion order keyed by the uniqueKey, nested `editions` / `_childDocuments_` children as separate rows with `_root_` and `_nest_path_ = /editions#i`, and a version that moves on every change. The Tantivy index (`src/index.rs`) is a derived snapshot, rebuilt by the first search after the version moves on. Up to four worker threads serve requests; an update holds the store's write lock for the whole request and saves before releasing it (Python saves on every update request, so nothing is lost on a kill), while searches only take an `Arc` of the current snapshot; one thread rebuilds it outside the store lock while the others wait for that version.

## What it supports

Endpoints, under `/solr/openlibrary/` unless noted, as Python minisolr's `app.py`:

- `select` and `query` (GET and POST form): `rows`, `start`, `fl`, `fq` (any number, parsed like `q`, score 0), function queries and `sort` (below), `cursorMark`, facets (below). Highlighting and spellcheck parameters are ignored.
- `select` and `query` with a JSON body (`Content-Type` containing `json`; the JSON Request API, as Python's `select_json`): the URL parameters, then `params` (arrays repeat a name; values as Python's `str()`), `query` (`q`; `*:*` when not a string), `filter` (each an `fq`), `limit` (`rows`), `offset` (`start`), `sort` and `fields` (`fl`; arrays joined with commas), and `facet` for the JSON Facet API (below), answered in `facets`. Malformed JSON is a 500, as in Python.
- `export`: a `select` returning every hit from `start` on, in `response.docs` (as Python, not Solr's export stream).
- `get`: `id` (repeatable) and `ids` (comma-separated), `fl` (names, globs; `*` includes docValues fields such as `trending_score_*`). One id answers `{"doc": ...}`, otherwise `response.docs` in request order; list values in stored order.
- `update`, `update/json`, `update/json/docs` (POST JSON): an array of documents, a single document, or a command object whose keys may repeat (`{"add": {...}, "add": {...}, "delete": {...}, "commit": {}}`, split by a small top-level scanner since `serde_json` keeps only the last repeated key). `add` takes `{"doc": ...}` or a bare document (either may be a list); a document missing the uniqueKey is rejected; a full add replaces the old family; atomic `set` (`null` removes the field), `inc`, `add`, `add-distinct`, `remove` (and `removeregex`, accepted and ignored as in Python) update a stored document, and fail when it does not exist; `delete` takes ids (string, `{"id"}` or lists) and `{"query"}` (Lucene syntax, no `df`), and deleting a root deletes its children. `commit`, `softCommit`, `{"commit": {}}` and `update.partial.requireInPlace` are accepted. Errors follow Python: a document error ends the request with status 400, unless `update.chain` names a tolerant chain, which records it in `responseHeader.errors` and goes on; anything else (bad JSON, a bad delete query) is always a 400. Unlike Python, a single bare document at the top level is added (Python ignores it).
- `mlt` (always empty, as Python), `admin/ping`, and `/solr/admin/cores` (`numDocs`).

`fl`: names and globs (`*` returns stored and docValues fields, as Solr's `useDocValuesAsStored`), `score`, `alias:field`, `[child]` (the rows whose `_root_` is the document's key, as `_childDocuments_`) and `alias:[subquery]` (the `alias.*` parameters plus `row.<field>` for each field of the document, run as a nested select; `alias.fl` splits on commas only), all as Python's `project`.

Query syntax, with Python minisolr's semantics (`src/query/`: `lexer`, `parser` and `ast`, then `compile`, `edismax`, `localparams`, `join`):

- **Lucene classic parser** (the default, or `{!lucene}`): terms, `"phrases"` with `~slop`, `field:term`, `field:"phrase"`, `field:(group)`, nested `( )` groups, `+ - !` prefixes, `AND OR NOT && ||` with the classic default-operator rules (`q.op`, `df`), `*:*`, bare `*`, `field:*` (exists), wildcards `* ?` and prefixes, fuzzy `term~` / `term~1`, boosts `^n` and constant scores `^=n`, backslash escapes, and ranges `[a TO b]`, `{a TO b}`, mixed and with `*` open ends: numeric, string, boolean and date fields, and enum fields in the order of `enumsConfig.xml` (`ebook_access:[borrowable TO *]`).
- **edismax** (`defType=edismax` or `{!edismax ...}`): `qf` (a `DisjunctionMax` per bare term), `pf`, `pf2`, `pf3`, `ps`, `qs`, `q.op`, `mm` (numbers, percentages, `n<rule` lists), unknown `field:` prefixes searched as text, clauses that analysis empties dropped.
- **Local params**: `{!edismax}`, `{!dismax}`, `{!lucene}`, `{!tag=...}`, `{!terms f= v= separator=}`, `{!field f=}`, `{!prefix f=}`, `{!parent which= v= filters=}` (an in-memory block join over `_root_`: the child query and filters run over every row and the parents of the matching children match with score 0), `v=` inline or as `$param`, quoted values, text after `}` as `v`, and `_query_:"{!...}"` nested queries.
- **Function queries** (`src/function/`): Python minisolr's `eval_fn` table, `sum`/`add`, `sub`, `mul`/`product`, `div`, `log` (base 10), `ln`, `sqrt`, `pow`, `abs`, `min`, `max`, `recip`, `if`, `def`, `exists`, numbers, `score`, field values (first value of a list; non-numbers are 0) and nested calls, plus `$param` arguments. An unknown function warns on stderr and evaluates to 0, as in Python. Each hit's score is multiplied by every edismax `boost=` expression (and by the request `boost` when `defType` is not edismax), then each `bf` term is added (`bf` is split on whitespace unless it contains `(`); boosts inside `fq` and `{!parent}` child queries are dropped. With a function or a sort, every hit is scored before the page is cut.
- **Sorting** (`src/query/sort.rs`): comma-separated keys with `asc`/`desc`, as Python's `apply_sort`: `score`, numeric fields, string fields (code point order; names outside the schema lowercased), enum fields in `enumsConfig.xml` order, `sort_text` (ICU collation) fields by folding, functions such as `def(trending_z_score, 0)`, and `random_<seed>` (by MD5 of the name and the key, `src/md5.rs`). Missing values sort last in both directions; index order breaks ties.

Facets (`src/query/facet.rs`), as Python's `facets` and `json_facets`, over every hit of `q` and the `fq`s (not only the returned page), counting each document once per distinct value (copyFields such as `subject_facet` write a value twice):

- **`facet=true`** (or `on`) or any `facet.field`: `facet.field` (repeatable; a leading `{!ex=...}` is ignored), `facet.mincount` (default 0), `facet.limit` (default 100; negative is unlimited), `facet.offset`, `facet.sort` (`count`, or `index`/`false`; default `count` unless the limit is 0), `facet.prefix`, each overridable per field as `f.<field>.facet.<name>`. With a mincount of 0 or less every value of a docValues string, enum, boolean or number field in the store is listed, zero counts included. Count order breaks ties by term (UTF-8 byte order). `facet.query` counts the hits a Lucene query (no `df`) also matches. Lists are `[[term, count], ...]` with `json.nl=arrarr`, otherwise flat `[term, count, ...]`; booleans are `true`/`false`, numbers as Python's `str()`. `facet.range`, `facet.pivot` and `facet.interval` warn and answer `{}`. The values of each faceted field are collected once per index snapshot (`FacetColumn`), so `facet.field` reads no documents.
- **JSON Facet API** (a JSON body's `facet`): `count` (the number of hits), `"sum(field)"` (the first number of the field in each hit that has it; an integer `0` when no hit has it), and `{"type": "terms", "field", "limit" (10), "offset", "mincount" (1), "sort"}` with `{"val", "count"}` buckets ordered by count then value, or by value with `"sort": "index"`. Other aggregations and facet types warn and are left out, as in Python. That covers what Open Library's author updater sends (ten `sum()`s and four `terms`).

`cursorMark`: as Python, `*` starts at 0, `o:<n>` at offset `n`, and `nextCursorMark` is `o:<start + returned>` (the cursor itself once a page is empty). No sort on the uniqueKey is required; ties break in index order, so pages are stable while the store does not change.

An undefined field, a bare term with no default field, a range on a text field, an unknown `{!type}`, malformed local params, a `json.facet` URL parameter and grouping return an empty result with a `warning` in `responseHeader` and a line on stderr. A field the schema defines but no document has matches nothing.

## Analyzers

`text_en_splitting` (whitespace, ICU-style folding, word delimiter with catenation, lowercase, protected words, Porter), `text_international` (standard tokenizer, folding) and `text_general` (standard tokenizer, lowercase). Not done: query-time synonyms; the stop lists are empty in the shipped config. `rust-stemmers` only ships Snowball English (Porter2), which differs from the original Porter that Solr and the Python code use (`odyssey` gives `odyssei`, `one` gives `on`), so `src/porter.rs` is a hand-written Porter (about 220 lines).

## Measurements (Apple Silicon Mac, host, no Docker, machine under swap pressure)

Facets, JSON APIs and `cursorMark` (step 5), all 267 requests, 1,300 queries on fresh servers started on a data directory, 5 runs: RSS 26.8-42.0 MiB, `footprint` 11-14 MiB, p50 1.53-1.65 ms, p95 3.9-5.7 ms. RSS swung more than before on this machine (9.7 of 11 GB swap in use): the step 6 binary measured in the same session on its 261 requests gave RSS 27.3-39.5 MiB, `footprint` 9.3-15 MiB, p50 1.59-1.62 ms, p95 4.8-5.4 ms, and the new binary on the same 261 RSS 24.8-27.4 MiB, `footprint` 11-14 MiB, p50 1.54-1.61 ms, p95 4.6-4.9 ms, so the spread is allocator and swap noise, not the facets. Stripped release binary 4.5 MiB.

Mutable store (step 6), the 261 answerable requests, 1,300 queries on fresh servers started on a data directory: RSS 25.9-26.9 MiB, `footprint` 12-13 MiB, p50 1.79-1.83 ms, p95 5.9-6.3 ms. The latency rise over step 3 is the `editions:[subquery]` transformer, which now runs (up to 10 nested selects in 109 of the 261 requests); with it stripped from `fl` the same requests measure RSS 23.4-24.0 MiB, `footprint` 12-13 MiB, p50 1.44-1.48 ms, p95 3.1-3.4 ms, as in step 3. After reindexing an empty server through `/update` (447 rows): RSS 5 MiB and `footprint` 4.5-4.9 MB before the first search (the store alone), 28-30 MiB and 10-12 MB once the index is built, and after 1,300 queries RSS 25.1-25.3 MiB, `footprint` 9.5-13 MiB. Stripped release binary 4.5 MiB.

Function queries and sorts (step 3), the 261 answerable requests, 1,300 queries on fresh servers: RSS 23.3-23.7 MiB, `footprint` 10-14 MiB; over 5,200 more queries RSS 20.4-23.2 MiB, `footprint` 9-13 MiB; p50 1.39-1.51 ms, p95 3.02-3.25 ms. The live malloc heap (`heap`) is 1.98 MB after load and 1.99 MB after 6,500 queries, with the same allocation count after 1,300 and 6,500 queries: the RSS above the 10 MiB at load is freed allocator memory macOS keeps resident, not growth (details in `PORTING.md`). Stripped release binary 4.4 MiB.

Query parser (step 2), the 247 answerable requests, 1,300 queries per run, fresh servers and repeated runs: RSS after 1,300 queries 17.7-22.8 MiB, `footprint` 11-12 MiB; RSS stays flat over 3,900 further queries (no growth); p50 1.33-1.51 ms, p95 2.98-3.27 ms. On the old 155-request subset: RSS 12.6 MiB, `footprint` 11 MiB, p50 1.32-1.39 ms, p95 2.96-3.37 ms. `footprint` (dirty memory) is where step 1 was; the higher RSS on the full set comes from the new query shapes (BOOK_SEARCH with a block join and many `qf`/`pf` fields) and is resident but reusable malloc memory. Stripped release binary 4.5 MiB.

Schema-driven version (step 1), same 155-request subset, 1,300 queries, two runs: RSS just after load 8-11 MiB, `footprint` 7-12 MiB; after 1,300 queries RSS 10.6 MiB, `footprint` 10-14 MiB; p50 1.35 ms, p95 2.9-3.0 ms. The hard-coded prototype measured in the same session: 9.8 MiB RSS and 12 MiB `footprint` after 1,300 queries, p50 1.11 ms, p95 2.7 ms. Parsing the schema and indexing every field did not move the memory number beyond noise. The table below is the original prototype measurement.

Same 155-request subset (1,300 queries), host to host. macOS compresses and swaps pages under pressure, so RSS jumps around; `footprint` is steadier.

| | Rust | Python minisolr |
|---|---|---|
| RSS just after load / first query | 14.6 MiB (27.8 on another run) | 42.4 MiB before first query, 79.5 right after index build |
| `footprint` idle | about 12 MiB | 55 MiB |
| RSS / footprint after 1,300 queries | 11.1 / 14 MiB | 48.7 / 56 MiB |
| latency p50 / p95 | 1.0-1.2 / 2.4-2.9 ms | 3.7 / 19.5 ms |

Stripped release binary 4.0 MiB; 125 crates; first release build 95 s with 2 jobs (largest compiler process 837 MiB RSS).

Correctness on the captured Open Library requests (`tools/replay.py`): all 267 are answerable; `numFound` equals real Solr's on all 267 and the top 10 is the same set on 258; top-10 key overlap 717/726 (98.76%), the same as Python minisolr; facets (the first 5 entries per facet field, as the e2e harness compares) equal on 6/6; `/get` 41/41. Against Python minisolr (`tools/compare_python.py`) numFound, the top-10 set and the top-10 order are identical on all 267, the whole `facet_counts` on 6/6, and the 109 requests with `editions:[subquery]` return identical documents. Feature probes (`--probes`, with every hard check of `e2e/probes.py`): all 57 select probes and the JSON Request API probe answerable, 56 of 58 matching (facet fields, facet queries, JSON facets and the cursor included); the 2 misses are the highlighting probes (not implemented yet); the 8 update, get and export probes match; numFound, keys, `start`, `nextCursorMark`, `facet_counts` and `facets` are identical to Python's on all 58. Step 6 answered 261, step 2 247 (overlap 91.81%, with boosts ignored), step 1 159.

## Limits

No highlighting, spellcheck or MoreLikeThis; JSON facets stop at `sum` and `terms` (no nested facets, `avg`, `min`, `max`, `unique`), as in Python; the whole store file is rewritten on every update request (as in Python) and the index is rebuilt in full after a change. Unicode categories are approximated with `char` methods and `unicode-normalization`, which cannot tell spacing marks (Mc) from nonspacing marks (Mn); that is the only remaining parity gap (some Telugu text): 2,467 of 2,470 index-time token lists match Python across all 23 text fields in the dev data (99.88%).
