# minisolr-rs

A Rust prototype of [minisolr](../minisolr)'s `/select`, first built to answer two questions: how much RAM does a Rust + Tantivy version really use, and can the Solr analyzer chains be reproduced token for token. It is now being ported step by step; [PORTING.md](PORTING.md) tracks the steps.

## Run

```bash
cargo run --release -- serve ../minisolr/conf ../minisolr/fixtures/docs.jsonl 127.0.0.1:8983
curl 'localhost:8983/solr/openlibrary/select?q=author_name:carroll&fq=type:work&fl=key'
cargo test --release
```

The first argument is a Solr conf directory with `managed-schema.xml` (and optionally `enumsConfig.xml`).

## Tools

All in `tools/`, Python. `replay.py` and `bench.py` use the standard library only and start nothing.

```bash
# correctness against real Solr's captured answers (the main progress meter)
python3 tools/replay.py http://127.0.0.1:8983/solr/openlibrary ../minisolr/fixtures [--probes] [-v] [--subset-out answered.json]
# memory and latency over the answerable requests
python3 tools/bench.py http://127.0.0.1:8983/solr/openlibrary <pid> answered.json 1300
# analyzer parity with Python minisolr, over every text field in the data
uv run --project ../minisolr python tools/dump_tokens.py ../minisolr/conf ../minisolr/fixtures/docs.jsonl > tokens.jsonl
cargo run --release -- parity ../minisolr/conf tokens.jsonl
```

## Schema

`src/schema.rs` reads `managed-schema.xml` and `enumsConfig.xml` (with `roxmltree`): field types (kind from the class, as in Python's `schema.py`), fields with Python's attribute defaults, dynamic fields (longest pattern first), copyFields, the uniqueKey and enum values. Every searchable static field is indexed, plus each dynamic field that occurs in the data; copyFields are applied at index time and when documents are returned. A text field type gets its analyzer from its index analyzer: `text_en_splitting`, `text_international` and `text_general`-shaped chains (and `text_gen_sort`, which has the same chain) are implemented; any other text type (`text_en`, the language types, `text_ws`, ...) falls back to whitespace + lowercase with a warning on stderr when a document uses it. None of the fields in the dev data need the fallback. Nested child documents are indexed as separate rows sharing `_root_` (no block join yet).

## What it supports

`/solr/openlibrary/select` (GET and POST form) and `/solr/openlibrary/admin/ping`. `rows`, `start`, `fl` (names and globs; `*` returns stored and docValues fields, as Solr's `useDocValuesAsStored`), `fq` (any number, parsed like `q`, score 0), ordering by score or by one numeric field (missing values last). Highlighting and spellcheck parameters are ignored.

Query syntax, with Python minisolr's semantics (`src/query/`: `lexer`, `parser` and `ast`, then `compile`, `edismax`, `localparams`, `join`):

- **Lucene classic parser** (the default, or `{!lucene}`): terms, `"phrases"` with `~slop`, `field:term`, `field:"phrase"`, `field:(group)`, nested `( )` groups, `+ - !` prefixes, `AND OR NOT && ||` with the classic default-operator rules (`q.op`, `df`), `*:*`, bare `*`, `field:*` (exists), wildcards `* ?` and prefixes, fuzzy `term~` / `term~1`, boosts `^n` and constant scores `^=n`, backslash escapes, and ranges `[a TO b]`, `{a TO b}`, mixed and with `*` open ends: numeric, string, boolean and date fields, and enum fields in the order of `enumsConfig.xml` (`ebook_access:[borrowable TO *]`).
- **edismax** (`defType=edismax` or `{!edismax ...}`): `qf` (a `DisjunctionMax` per bare term), `pf`, `pf2`, `pf3`, `ps`, `qs`, `q.op`, `mm` (numbers, percentages, `n<rule` lists), unknown `field:` prefixes searched as text, clauses that analysis empties dropped.
- **Local params**: `{!edismax}`, `{!dismax}`, `{!lucene}`, `{!tag=...}`, `{!terms f= v= separator=}`, `{!field f=}`, `{!prefix f=}`, `{!parent which= v= filters=}` (an in-memory block join over `_root_`: the child query and filters run over every row and the parents of the matching children match with score 0), `v=` inline or as `$param`, quoted values, text after `}` as `v`, and `_query_:"{!...}"` nested queries.
- **Parsed but ignored** until step 3: edismax `boost=`, the request `boost` and `bf` (so ranking is plain BM25; `FunctionScoring` in `src/query/mod.rs` is where the function evaluator plugs in). The `[subquery]` and `[child]` `fl` transformers are ignored too (they do not change `numFound`).

An undefined field, a bare term with no default field, a range on a text field, an unknown `{!type}`, malformed local params, facets, grouping, cursors and other sorts return an empty result with a `warning` in `responseHeader` and a line on stderr. A field the schema defines but no document has matches nothing.

## Analyzers

`text_en_splitting` (whitespace, ICU-style folding, word delimiter with catenation, lowercase, protected words, Porter), `text_international` (standard tokenizer, folding) and `text_general` (standard tokenizer, lowercase). Not done: query-time synonyms; the stop lists are empty in the shipped config. `rust-stemmers` only ships Snowball English (Porter2), which differs from the original Porter that Solr and the Python code use (`odyssey` gives `odyssei`, `one` gives `on`), so `src/porter.rs` is a hand-written Porter (about 220 lines).

## Measurements (Apple Silicon Mac, host, no Docker, machine under swap pressure)

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

Correctness on the captured Open Library requests (`tools/replay.py`): 247 of 267 are answerable (every request except 14 function or random sorts and 6 facet requests); `numFound` equals real Solr's on all 247 and the top 10 is the same set on 226; top-10 key overlap 639/696 (91.81%). Python minisolr gets 687/696 on the same 247. All 21 misses are BOOK_SEARCH requests with more than 10 hits where the ignored `boost` function decides the order: with Python's function boosts neutralized, the Rust answers equal Python's on all 247 (numFound, top-10 set and top-10 order). Step 1 answered 159 (146 top-10 equal).

## Limits

Single-threaded `tiny_http`, no persistence, no updates, no facets. Unicode categories are approximated with `char` methods and `unicode-normalization`, which cannot tell spacing marks (Mc) from nonspacing marks (Mn); that is the only remaining parity gap (some Telugu text): 2,467 of 2,470 index-time token lists match Python across all 23 text fields in the dev data (99.88%).
