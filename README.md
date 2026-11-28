# minisolr-rs

A small Solr stand-in for Open Library development, written in Rust on [Tantivy](https://github.com/quickwit-oss/tantivy). It answers the Solr requests Open Library's dev stack makes (`make reindex-solr`, the site, the `solr-updater` daemon) from a 7.67 MB image that idles at about 5 MiB of RAM, where real Solr needs about 250 MiB. It began as a port of a Python prototype that has since been retired. [PORTING.md](PORTING.md) records how it was built and measured.

## Quick start

From your Open Library checkout:

```bash
curl -fsSL https://raw.githubusercontent.com/RayBB/minisolr-rs/main/install.sh | sh          # use minisolr-rs
docker compose up -d && docker compose run --rm home make reindex-solr
curl -fsSL https://raw.githubusercontent.com/RayBB/minisolr-rs/main/install.sh | sh -s off   # back to real Solr
```

`install.sh` downloads one compose file to `~/.minisolr/` and sets `COMPOSE_FILE` in Open Library's git-ignored `.env`; nothing tracked in the checkout changes. The engines use separate data volumes, so reindex after switching. Needs Docker Compose 2.24 or newer.

## Run

```bash
mkdir -p /tmp/ms && cp fixtures/docs.jsonl /tmp/ms/
cargo run --release -- serve conf /tmp/ms 127.0.0.1:8983
curl 'localhost:8983/solr/openlibrary/select?q=author_name:carroll&fq=type:work&fl=key'
curl -X POST 'localhost:8983/solr/openlibrary/update?commit=true' -H 'Content-Type: application/json' \
  -d '{"add": {"doc": {"key": "/works/OL1W", "type": "work", "title": "x"}}, "commit": {}}'
cargo test --release
python3 tools/check.py   # replay the captured Open Library requests and the 82 probes; the CI gate
```

The first argument is a Solr conf directory with `managed-schema.xml` (and optionally `enumsConfig.xml`). The second is the data directory (created if missing): the store is loaded from `<dir>/docs.jsonl` and saved there after every update request that changes something, in the same format as Python minisolr, so either server can open the other's directory. Given a `.jsonl` file instead, the server loads it and keeps updates in memory only. `minisolr-rs ping [addr]` exits 0 when the server at `addr` (default `127.0.0.1:8983`) answers `/solr/openlibrary/admin/ping`; the image uses it as its healthcheck.

## Pull the published image

Every push to `main` builds a multi-architecture (amd64 and arm64) image and publishes it to GitHub Container Registry; a release tag such as `v1.2.3` adds version tags.

```bash
docker pull ghcr.io/raybb/minisolr-rs:latest
```

To run Open Library's dev stack on it with no clone and no local build, download one compose file and use it as the `solr` override (needs Docker Compose 2.24 or newer):

```bash
cd ~/workspace/openlibrary
curl -O https://raw.githubusercontent.com/RayBB/minisolr-rs/main/compose.ghcr.yaml
docker compose -f compose.yaml -f compose.override.yaml -f compose.ghcr.yaml up -d
docker compose -f compose.yaml -f compose.override.yaml -f compose.ghcr.yaml run --rm home make reindex-solr
```

To make plain `docker compose up` use it, put `COMPOSE_FILE=compose.yaml:compose.override.yaml:compose.ghcr.yaml` in Open Library's git-ignored `.env`. Tags: `latest` (the `main` branch), `x.y.z` and `x.y` (releases), `sha-<commit>`. Pin one with `MINISOLR_IMAGE_TAG`. The image is built by `.github/workflows/docker.yml` and is MIT licensed like the code.

## Docker and Open Library's dev stack

The `Dockerfile` builds a static musl binary in `rust:1-alpine` (two compile jobs, dependencies in their own cached layer) and copies it into a `scratch` image of about 8 MB. The container reads the schema from `/conf`, keeps its documents in `/data/docs.jsonl` and listens on port 8983.

`compose.minisolr-rs.yaml` replaces Open Library's `solr` service with it, as Python minisolr's `compose.minisolr.yaml` does. In the Open Library checkout, add two lines to `.env`:

```bash
MINISOLR_RS_DIR=/path/to/minisolr-rs
COMPOSE_FILE=compose.yaml:compose.override.yaml:/path/to/minisolr-rs/compose.minisolr-rs.yaml
```

Then `docker compose up -d` and `docker compose run --rm home make reindex-solr` as usual. To go back to real Solr, remove both lines. The engines use separate volumes (`minisolr-rs-data`, `solr-data`), so reindex after switching. The service runs with `init: true` so stops are immediate; every update request is written to disk (fsync, then rename) before it is answered, so a stop or restart never loses committed documents.

The end-to-end harness runs the complete Open Library dev stack on this image (reindex, smoke test, replay, restart, probes, the real `solr-updater`); it needs an Open Library checkout and Docker:

```bash
OL_DIR=/path/to/openlibrary python3 e2e/harness.py all-minisolr --down
```

The repository carries what it is checked against: `conf/` is a copy of Open Library's Solr schema (AGPL, see `conf/README.md`), `fixtures/` holds the 447-document dev set, the 267 captured requests and real Solr's answers to them, and `e2e/` the harness.

## Tools

All in `tools/`, Python. `replay.py`, `roundtrip.py`, `probes_compare.py` and `bench.py` use the standard library only (`probes_compare.py --binary` starts its own server); `gen_unicode.py` regenerates the Unicode tables from Python's `unicodedata`.

```bash
# correctness against real Solr's captured answers (the main progress meter)
python3 tools/replay.py http://127.0.0.1:8983/solr/openlibrary fixtures [--probes] [-v] [--subset-out answered.json]
# reindex an empty server through /update (works with nested editions, repeated `add` keys), then compare two servers
python3 tools/roundtrip.py load http://127.0.0.1:8984/solr/openlibrary fixtures/docs.jsonl
python3 tools/roundtrip.py compare http://127.0.0.1:8983/solr/openlibrary http://127.0.0.1:8984/solr/openlibrary fixtures
# all 66 e2e probes through e2e/probes.py's compare(), on a server started from a copy of the fixtures
python3 tools/probes_compare.py . --binary target/release/minisolr-rs   # or --url http://...
# memory and latency over the answerable requests
python3 tools/bench.py http://127.0.0.1:8983/solr/openlibrary <pid> answered.json 1300
# regenerate the Unicode tables from Python's unicodedata
python3 tools/gen_unicode.py > src/unicode/table.rs
```

## Schema

`src/schema.rs` reads `managed-schema.xml` and `enumsConfig.xml` (with `roxmltree`): field types (kind from the class, as in Python's `schema.py`), fields with Python's attribute defaults, dynamic fields (longest pattern first), copyFields, the uniqueKey and enum values. Every searchable static field is indexed, plus each dynamic field that occurs in the data; copyFields are applied at index time and when documents are returned. A text field type gets an index-time and a query-time analyzer chain from its `<analyzer>` elements (see Analyzers below), as Python does; a type without a query analyzer analyzes queries as one token (Python does not fall back to the index analyzer). Nested child documents are separate rows sharing `_root_`.

## Store, updates and concurrency

The store (`src/store.rs`) is the source of truth, as in Python: documents in insertion order keyed by the uniqueKey, nested `editions` / `_childDocuments_` children as separate rows with `_root_` and `_nest_path_ = /editions#i`, and a version that moves on every change. The Tantivy index (`src/index.rs`) is a derived snapshot, rebuilt by the first search after the version moves on. Up to four worker threads serve requests; an update holds the store's write lock for the whole request and saves before releasing it (Python saves on every update request, so nothing is lost on a kill), while searches only take an `Arc` of the current snapshot; one thread rebuilds it outside the store lock while the others wait for that version.

## What it supports

Endpoints, under `/solr/openlibrary/` unless noted, as Python minisolr's `app.py`:

- `select` and `query` (GET and POST form): `rows`, `start`, `fl`, `fq` (any number, parsed like `q`, score 0), function queries and `sort` (below), `cursorMark`, facets, highlighting and spellcheck (below). `mlt=true` is ignored with a line on stderr, as in Python.
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

Highlighting (`hl=true`, `src/query/highlight.rs`), as Python's `highlighting`: the terms of `hl.q` (or `q`; local params and `$param`s blanked) after each `hl.fl` text field's query analyzer; in the returned page, every word (`\w+(?:['’]\w+)*`) of each value of the field (copyFields applied) whose own analysis shares a term is wrapped in `<em>...</em>`, and every value with a match is a snippet (the whole value), at most `hl.snippets` (default 1; Python's slice semantics, so 0 gives an empty list and a negative number drops from the end) per field, keyed by the document's `key`. Python ignores `hl.fragsize`, `hl.simple.pre`/`hl.simple.post`, `hl.requireFieldMatch` and the other options, and so does this.

Spellcheck (`spellcheck=true`), as Python's `spellcheck`: the words of `spellcheck.q` (or of `userWorkQuery`, else `q`, with local params, `$param`s and punctuation blanked), split on whitespace; each that folds to 4 or more characters and is not in the dictionary (every whitespace-separated word longer than 3 characters of `title`, `author_name` and `name`, folded; built with the index) gets `difflib.get_close_matches(word, sorted dictionary, spellcheck.count (5), 0.6)`, in `suggestions` as `[word, {numFound, startOffset, endOffset, suggestion}, ...]` (offsets in characters); `collations` is always empty. `src/difflib.rs` ports `SequenceMatcher` (`find_longest_match`, the matching-block recursion, `ratio`, the two quick ratios, autojunk) and `get_close_matches` (ties ranked by the larger string first, as Python ranks `(ratio, word)` tuples).

`cursorMark`: as Python, `*` starts at 0, `o:<n>` at offset `n`, and `nextCursorMark` is `o:<start + returned>` (the cursor itself once a page is empty). No sort on the uniqueKey is required; ties break in index order, so pages are stable while the store does not change.

A bare term with no default field, a range on a text field, an unknown `{!type}`, malformed local params, a `json.facet` URL parameter and grouping return an empty result with a `warning` in `responseHeader` and a line on stderr. An undefined field answers HTTP 400 like real Solr (see Limits). A field the schema defines but no document has matches nothing.

## Analyzers

`src/analysis.rs` builds every chain from the schema's `<analyzer>` elements, as Python's `analysis.py`: the `whitespace` (Python's `str.split()`) and `keyword` tokenizers, and the simplified standard tokenizer for any other name; the filters `lowercase`, `stop` (the `words` file, `ignoreCase`), `icuFolding` (NFKD, drop nonspacing marks, case fold, a few extra letters, NFC), `wordDelimiterGraph` (`generateWordParts`, `generateNumberParts`, `catenateWords`, `catenateNumbers`, the `types` file; parts take successive positions, catenations stack on the first, `'s` dropped), `porterStem` (`src/porter.rs`, the original Porter that Solr and Python use; `rust-stemmers` has only Porter2), `snowballPorter` (`src/snowball.rs`, Snowball English for every language, as Python), `englishPossessive`, `englishMinimalStem`, `keywordMarker` (`protwords.txt`), `synonymGraph` and `removeDuplicates`. Word lists are read from the files the filters name (a missing file is an empty list). Positions hold alternatives: queries OR the alternatives of a position, and a phrase over positions with alternatives is the OR of the first 8 combinations, as Python. Synonyms follow Python's approximation: rules with a multi-word term are ignored, `a => b` replaces the token, groups stack the other terms on its position (`expand=true`) or replace it with the first (`expand=false`). Components Python does not implement (language stemmers and normalizers, CJK bigrams, phonetic, payloads, the Japanese, Korean and path tokenizers) are skipped, as in Python, and listed in one warning line at startup naming each field type. Character categories, case folding and Python's whitespace come from tables generated from Python's `unicodedata` (`src/unicode/`, Unicode 16.0), so spacing marks (Mc) are kept by folding as in Python.

## Measurements in Docker (Open Library's dev stack, OrbStack on an Apple Silicon Mac)

`e2e/harness.py all-minisolr --down` with this compose override, two complete runs, every checklist item PASS in both: `make reindex-solr` 446 documents (35 works, 68 editions, 34 authors through the JSON Request/Facet API, 309 subjects), site smoke test 86/86, replay `numFound` 267/267, top-10 overlap 98.76%, facets 6/6, `/get` 41/41, the same after `docker compose restart solr`, probes 66/66 against real Solr (the same 10 soft notes as Python), and the real `solr-updater` daemon (an edited work and a new list searchable after 9 s, a deleted list gone).

| | minisolr-rs | Python minisolr |
|---|---|---|
| Container memory idle after restart (`docker stats`) | 3.5-3.6 MiB | 38-41 MiB |
| after 1,335 replayed selects | 4.7-4.9 MiB | 42-43 MiB |
| latency p50 / p95 / mean (harness, host to container) | 2.6-2.7 / 7.5-8.0 / 3.4-3.5 ms | 4.4-5.2 / 23-32 ms |
| image | 7.67 MB (`scratch` + 7.6 MB static binary) | 126 MB |
| restart until ping answers | 0.6 s | |

The idle number is the store alone (the index is built by the first search). First image build 3 min 33 s (45 s pulling `rust:1-alpine`, 2 min 24 s for the dependencies with two jobs, 15 s for the crate); rebuilds after a source change reuse the dependency layer. The build ran on a machine with 16 GB of RAM and 9-10 GB of swap in use without trouble.

## Measurements (Apple Silicon Mac, host, no Docker, machine under swap pressure)

Analyzers from the schema, highlighting and spellcheck (steps 7 and 8), all 267 requests (37 with highlighting, 211 with spellcheck), 1,300 queries on fresh servers started on a data directory, 5 runs, 9.0 of 10 GB swap in use: RSS 24.7-40.6 MiB, `footprint` 11-13 MiB, p50 1.62-1.76 ms, p95 4.7-5.6 ms. Stripped release binary 4.6 MiB (the Unicode tables add about 0.1 MiB).

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

Correctness on the captured Open Library requests (`tools/replay.py`): all 267 are answerable; `numFound` equals real Solr's on all 267 and the top 10 is the same set on 258; top-10 key overlap 717/726 (98.76%), the same as Python minisolr; facets (the first 5 entries per facet field, as the e2e harness compares) equal on 6/6; `/get` 41/41. Against the retired Python prototype numFound, the top-10 set and the top-10 order are identical on all 267, the whole `facet_counts` on 6/6, and the 109 requests with `editions:[subquery]` return identical documents. The `highlighting` section is identical to Python's on all 37 requests that ask for it and `spellcheck` on all 211 (48 with suggestions). Feature probes (`--probes`, with every hard check of `e2e/probes.py`): all 57 select probes and the JSON Request API probe answer and match, 58/58 (facet fields, facet queries, JSON facets, the cursor, highlighting and spellcheck included); the 8 update, get and export probes match; numFound, keys, `start`, `nextCursorMark`, `facet_counts`, `facets`, `highlighting` and `spellcheck` are identical to Python's on all 58. All 66 probes through `e2e/probes.py`'s own `compare()` (`tools/probes_compare.py`): `probes: 66/66 match real Solr (hard checks); 0 failing, 10 soft differences`, the same line and the same 10 notes as Python minisolr. Analyzer parity with Python: 2,470/2,470 index-time token lists over the 23 text fields of the dev data, and 192,794/192,794 index and query analyses (positions and alternatives) over 4,102 inputs for each of the 47 text field types the schema uses (every distinct value and token of the dev data plus crafted strings: synonyms, possessives, accents, hyphens, digit/letter mixes, case changes, CJK, Cyrillic, Arabic, Indic and other scripts, odd whitespace); 1,038,636/1,038,636 over the 259,659 words of `/usr/share/dict/words` (as given and lowercased) for the Porter, Snowball English and minimal-stem chains. Step 6 answered 261, step 2 247 (overlap 91.81%, with boosts ignored), step 1 159.

## Limits

As in Python: MoreLikeThis returns nothing (`/mlt` is empty, `mlt=true` is ignored), highlighting returns whole values rather than fragments, spellcheck has no collations, analyzer components Python lacks are skipped, and JSON facets stop at `sum` and `terms` (no nested facets, `avg`, `min`, `max`, `unique`); the whole store file is rewritten on every update request and the index is rebuilt in full after a change. Unlike Python, a field the schema does not define is an error, as in real Solr (checked against Solr 10.1 with `e2e/probes.py`): HTTP 400 with `error.msg` `undefined field nosuch` for a query clause (`q`, `fq`, `df`, wildcards, `{!terms}`, facet queries, `delete` by query), `undefined field: \"nosuch\"` for a range, `{!field}`, `{!prefix}`, `facet.field` and JSON facets, `sort param field can't be found: nosuch` for `sort`, and `Query Field 'nosuch' is not a valid field name` for an edismax `qf`. Python logs a warning and treats just that clause as matching nothing. Edismax still reads `nosuch:foo` as text, ignores an undefined `pf` field and a range on one, and `fl` ignores unknown names, all as Solr does. Unlike Solr, a failing facet or highlighting request carries no partial `response`, and an undefined field inside `bf`, `boost` or a sort function is not rejected. Other features Python only warns about (a bare term with no default field, a range on a text field, an unknown `{!type}`) still return an empty result with a `warning`.

## Credits and license

MIT, see [LICENSE](LICENSE). Built with Claude Code.
