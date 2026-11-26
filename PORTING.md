# Porting minisolr to Rust: progress and plan

Status: **prototype done; steps 1 (schema), 2 (query parser, edismax) and 3 (function queries, boost) done, plus the sorts of step 4. The go/no-go checkpoint is [below](#checkpoint-after-step-3).** This file tracks how far the Rust version is from replacing the Python one in [minisolr](../minisolr), what the prototype showed, and what to do next. Numbers marked *(est.)* are estimates, not measurements.

## Why we are doing this

Python minisolr already saves about 80% of Solr's memory (38/43 MiB in Docker against about 250 MiB). The question was whether a Rust version is worth the effort. A Go port was ruled out (the Tantivy binding is too thin). A Rust study estimated a saving of 15-25 MiB; the prototype below measured more.

## What the prototype showed

| Question | Answer | Evidence |
|---|---|---|
| Memory | about 12-14 MiB footprint against about 55 MiB for Python (host to host, noisy) | `README.md` table |
| Latency | p50 1.0-1.2 ms, p95 2.4-2.9 ms against 3.7 / 19.5 ms | same subset, 1,300 queries |
| Binary | 4 MiB stripped release binary | |
| Build cost | 95 s first release build, 2 jobs, largest process 837 MiB | 125 crates, mostly from tantivy |
| Analyzer parity | 2,114 of 2,117 index-time token lists identical (99.86%); every `text_en_splitting` and `text_general` field 100% | 16 fields, all 447 dev docs |
| Query correctness | 155 of 267 captured requests answerable; `numFound` equals Solr on 155/155; top-10 set equal on 142/155 | Python minisolr: 155/155 and 147/155 |
| Quality gates | clippy pedantic `-D warnings`, `cargo fmt`, 15 tests pass, no `unwrap` outside tests | |

Findings that shape the port:

- **Same Tantivy.** `tantivy-py` 0.27 wraps Tantivy 0.27, so BM25 scores are the same engine; the top-10 differences come from the ignored `boost` function, not scoring.
- **`rust-stemmers` is the wrong stemmer.** It only ships Snowball English (Porter2). Solr's `porterStem` is the original Porter, so `odyssey` stems to `odyssei`, not `odyssey`. The hand-written `src/porter.rs` (about 220 lines) fixes it; `alternative_title` went from 73.8% to 100%.
- **Unicode marks.** `unicode-normalization` cannot tell spacing marks (Mc) from nonspacing marks (Mn). That causes the 3 remaining token mismatches (one Telugu name). It needs a small category table.

## What is covered and what is not

Covered (after step 3): `/select` (GET and POST) and `/admin/ping`; the schema from `managed-schema.xml` and `enumsConfig.xml`; the Lucene classic parser, edismax, local params (`{!edismax}`, `{!lucene}`, `{!terms}`, `{!field}`, `{!prefix}`, `{!parent}`), `$param` dereference and `_query_`, with Python minisolr's semantics (details in `README.md`); `fq`; `rows`, `start`, `fl`; function queries in edismax `boost`, the request `boost` and `bf`; multi-key `sort` on score, fields (numbers, strings, enum order, folded `sort_text`), functions and `random_*`; three analyzer chains (`text_en_splitting`, `text_international`, `text_general`).

The prototype could not answer 112 captured requests. What they needed:

| Needs | Requests | Fixed by step |
|---|---|---|
| Multi-clause `+(...)`, boosted constants, subqueries, `OR` | 76 | 2 (done) |
| `osp_count` and `trending_score_hourly_sum` fields (not in the hard-coded table) **and** range syntax (`[1 TO *]`, `OR`) | 14 | 1 for the fields, 2 for the ranges (done; the schema alone did not unlock them, see step 1 results) |
| Sorts on functions, `random_*` or non-table fields | 16 | 4 for the 14 left (the 2 trending carousel requests now fail only on `def(trending_z_score, 0)`; 4 `ratings_sortable` sorts were fixed by step 1) |
| Facets | 6 | 5 |

Still unanswered after step 3 (6): the 6 facet requests (step 5).

Also not done: `cursorMark`, `/update`, persistence, `/get`, `/export`, the `[subquery]` and `[child]` transformers, JSON Request/Facet API, highlighting, spellcheck, query-time synonyms.

## Next steps, in order

Each step has a measurable gate: the language-independent fixtures in `../minisolr/fixtures` (267 replayed requests with real Solr's answers, 66 probes) and the end-to-end harness in `../minisolr/e2e`.

1. [x] **Read the schema.** Parse `managed-schema.xml` and `enumsConfig.xml` (fields, dynamic fields, copyFields, types, enums) instead of the hard-coded table. Likely adds one small XML crate (`roxmltree`). *Gate: the 14 `osp_count`/trending requests answer (wrong as planned: they also need ranges, which step 2 added); parity still 99.86% or better.* About 1-2 days *(est.)*.
   **Done** (`src/schema.rs`, `roxmltree` 0.20, no other crates). Measured:
   - Replay (`tools/replay.py`): 159/267 answerable (was 155), `numFound` equal 159/159, top-10 set equal 146/159 (was 142), key overlap 474/510 = 92.94%. The 4 new answers sort on `ratings_sortable`.
   - The 14 `osp_count`/trending requests do **not** answer yet, and could not with a schema alone: the prototype rejected them on the unknown field first, but all 14 also use range syntax. 12 have `fq=osp_count:[1 TO *] OR readinglog_count:[5 TO *] OR edition_count:[5 TO *]` (ranges plus `OR`), 2 have `trending_score_hourly_sum:[1 TO *] AND readinglog_count:[4 TO *]`, `fq=ebook_access:[borrowable TO *]` (an enum range) and `sort=def(trending_z_score, 0) desc`. They move to step 2 (and step 4 for the two function sorts).
   - Analyzer parity now covers every text field in the dev data, 23 fields including the `title_suggest` copyField: 2,467/2,470 = 99.88% (was 2,114/2,117 over 16 fields); the 3 misses are the known Telugu Mc/Mn case.
   - Memory, same 155-request subset, 1,300 queries: RSS 10.6 MiB, `footprint` 10-14 MiB (hard-coded prototype in the same session: 9.8 MiB, 12 MiB); unchanged within noise. p50 1.35 ms, p95 3.0 ms (prototype 1.11 / 2.7 ms in the same session).
   - Unanswered now (108): 67 `+({!edismax ...})` multi-clause queries, 12 `osp_count` range + `OR` filters, 3 `ebook_access:[borrowable TO *]`, 3 `ia:*`, 3 bare `OR`, 2 trending ranges (step 2); 4 `def(first_publish_year, 9999)`, 4 `def(trending_z_score, 0)`, 4 `random_1` sorts (step 4); 6 facets (step 5).
2. [x] **Lucene query parser and edismax.** Classic boolean semantics (`+ - AND OR NOT`, groups, ranges, wildcards, fuzzy, slop), local params (`{!edismax}`, `{!terms}`, `{!field}`, `{!prefix}`), `$param` dereference, `qf`/`pf`/`pf2`/`pf3`, `q.op`, `mm`. The biggest piece (Python's `query.py` is 624 lines). *Gate: the 76 multi-clause requests answer.* About 4-6 days *(est.)*.
   **Done** (`src/query/`: `lexer.rs`, `ast.rs`, `parser.rs`, `localparams.rs`, `compile.rs`, `edismax.rs`, `join.rs`; no new crates). The AST is built once per query string and compiled to Tantivy term, phrase (slop), boolean, boost, const-score, disjunction-max, range, regex, fuzzy, term-set and all queries, as Python does. Also `{!parent}` as an in-memory block join over `_root_`, and `ps`/`qs` (default 0, which is all Python does). Measured:
   - Replay: 247/267 answerable (was 159), `numFound` equal 247/247, top-10 set equal 226/247, key overlap 639/696 = 91.81%. The 20 unanswered are the 14 function/random sorts (step 4) and 6 facet requests (step 5): every request not blocked by those. That is 2 fewer than the 249 expected, because the 2 trending carousel requests also sort on `def(trending_z_score, 0)`.
   - All 21 top-10 misses are BOOK_SEARCH requests with more than 10 hits ordered by the ignored edismax `boost` function. Python minisolr gets 687/696 on the same 247; with Python's function boosts neutralized, the Rust answers equal Python's on all 247 (numFound, top-10 set and order). So the parser matches; the gap is step 3.
   - Probes (`--probes`): 40 of 57 select probes answerable, 38 match; the 2 failures are lifecycle probes that need `/update`; the 17 unanswered are facets, sorts and `cursorMark`.
   - Analyzer parity unchanged: 2,467/2,470.
   - Memory and latency on the 247 answerable requests (the old 155-request subset no longer covers what is supported), 1,300 queries: RSS 17.7-22.8 MiB, `footprint` 11-12 MiB, flat over 3,900 more queries; p50 1.33-1.51 ms, p95 2.98-3.27 ms. Old 155 subset on a fresh server: RSS 12.6 MiB, `footprint` 11 MiB, p50 1.39 ms, p95 3.37 ms. Binary 4.5 MiB.
   - Gates: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` (pedantic), `cargo test --release` (56 unit tests, 3 HTTP tests).
   - For step 3: `FunctionScoring` in `src/query/mod.rs` holds the edismax `boost=` expressions (collected while compiling, in `Compiler::boosts`), the request `boost` (only when `defType` is not edismax) and `bf`. Python multiplies each hit's score by every boost expression, then adds each `bf` term (`bf` split on whitespace unless it has a `(`), and orders by the result, so the search must score all hits (not only the top `rows`) whenever a function is present. Boosts inside `fq` and the `{!parent}` child query are discarded, as in Python.
3. [x] **Function queries and boost.** `sum`, `mul`, `div`, `log10`, `def`, `min`, `max`, `field`, `recip`, applied after scoring, plus `bf`. *Gate: top-10 sets equal on the 13 requests that missed; overlap at or above Python's 98.76%.* About 1-2 days *(est.)*.
   **Done** (`src/function/`: `ast.rs`, `parse.rs`, `eval.rs`, `mod.rs`; no new crates). Python's `eval_fn` read the same way (a name, then everything to the last `)`; arguments split on commas outside parentheses; otherwise `score`, a number or a field) and its whole table: `sum`/`add`, `sub`, `mul`/`product`, `div` (0 on division by 0), `log` (base 10) and `ln` (0 for x <= 0), `sqrt`, `pow`, `abs`, `min`, `max`, `recip`, `if`, `def` (a field with a default; a non-field first argument is evaluated and the default ignored), `exists`. Fields read the stored row, first value of a list, numbers only. An unknown function warns on stderr and is 0, as in Python. Added over Python: a `$name` argument is replaced by the request parameter. Each hit's score is multiplied by every `boost` expression in turn (edismax `boost=`, plus the request `boost` when `defType` is not edismax), then each `bf` term is added (split on whitespace unless it contains `(`); boosts inside `fq` and the `{!parent}` child query are dropped. With a function or a sort, every hit is scored before the page is taken. Measured: see the checkpoint.
4. [ ] **Sorting.** Field sorts, function sorts, `random_*` (MD5-based as in Python), ICU-style sort keys, enum order. *Gate: the 16 sort requests.* About 1 day *(est.)*.
   - [x] **Done with step 3** (`src/query/sort.rs`, `src/md5.rs`): Python's `apply_sort` (comma-separated keys, direction after the last space, one stable sort per key from last to first starting in index order, missing values last in both directions), on `score`, numbers, strings (code point order; names outside the schema lowercased), enums in `enumsConfig.xml` order, `sort_text` fields by the same folding as the analyzers, functions (`def(trending_z_score, 0)`, `def(first_publish_year, 9999)`) and `random_<seed>` (first 4 bytes of the MD5 of the name and the document key, a hand-written 90-line MD5 checked against the RFC 1321 vectors). All 14 sort requests and the 6 sort probes answer.
   - [ ] Left: `cursorMark` (1 probe). Real ICU collation is not planned; Python approximates it the same way.
5. **Facets.** `facet.field` (mincount, sort, offset, limit, prefix, zero-count terms, per-document distinct counts), `facet.query`, `json.nl=arrarr`, then the JSON Request and Facet APIs the author updater uses. *Gate: 6/6 facet requests; real `make reindex-solr` indexes authors.* About 2 days *(est.)*.
6. **Updates and persistence.** `/update` (repeated-key command bodies, arrays, atomic `set add add-distinct remove inc`, delete by id and query, commit), JSONL store with lazy index rebuild, `/get`, `/export`, nested `editions` children with the in-memory `{!parent}` block join and `[subquery]`/`[child]`. *Gate: 447-doc reindex, lifecycle probes, `/get` 41/41, restart persistence.* About 3-4 days *(est.)*.
7. **Remaining analysis.** Query-time synonyms and `synonymGraph`, full `wordDelimiterGraph` positions, a Mc/Mn category table, non-English chains if any field needs them. *Gate: analyzer parity 100% on all fields.* About 1-2 days *(est.)*.
8. **Highlighting and spellcheck** (basic versions, as in Python). About 1 day *(est.)*.
9. **Packaging.** Static musl build, scratch or Alpine image, compose override that selects the Rust image, an `--engine` switch in the e2e harness. *Gate: `all-minisolr` passes with the Rust server: smoke 86/86, replay 267/267, probes 66/66, `solr-updater` daemon, restart persistence.* About 1-2 days *(est.)*.

Remaining total after step 3: roughly **8-12 working days** *(est.)*: facets 2, updates, persistence and nested documents 3-4, analysis 1-2, highlighting and spellcheck 1, packaging 1-2, `cursorMark` under half a day. The original estimate for steps 1-3 was 6-10 days.

## Checkpoint after step 3

The go/no-go checkpoint planned below ("after step 3, rerun the replay; stop if the answerable share is not near 100% or memory has drifted up"). Measured on the Apple Silicon Mac, host, no Docker, under heavy swap pressure (8.8 of 10 GB swap in use).

| Measure | Result | Bar |
|---|---|---|
| Replay answerable | **261 of 267**; the 6 left are the facet requests (step 5) | near 100% |
| `numFound` equal to Solr | 261/261 | 267/267 at the end |
| Top-10 set equal to Solr | 252/261; key overlap 715/724 = **98.76%** | at or above 90%; Python 98.76% |
| Same answers as Python minisolr | numFound, top-10 set **and top-10 order identical on 261/261** (`tools/compare_python.py`); with Python's boosts neutralized only 216/261 orders agree, so the boosts are exercised | |
| Probes (`--probes`) | 49 of 57 select probes answerable, 47 match; the 2 failures need `/update` (step 6); unanswered: 7 facets, 1 `cursorMark` | 66/66 at the end |
| Analyzer parity | 2,467/2,470 (99.88%), unchanged | 100% after step 7 |
| Memory, 1,300 queries over the 261 | RSS 23.3-23.7 MiB, `footprint` 10-14 MiB (fresh servers); RSS 20.4-23.2 MiB, `footprint` 9-13 MiB over 5,200 more | below 20 MiB |
| Latency | p50 1.39-1.51 ms, p95 3.02-3.25 ms | p95 below 10 ms |
| Binary | 4.4 MiB stripped; crates unchanged (tantivy, serde_json, unicode-normalization, tiny_http, roxmltree) | |
| Code | 6,136 lines in `src/` with tests and doc comments; 3,542 lines of code outside tests. Python minisolr: 1,936 lines (1,644 of code) for the whole server, including facets, updates, persistence, highlighting and spellcheck. Step 3 added about 1,290 lines (with the HTTP test), about 40% of them tests | |
| Gates | `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` (pedantic), `cargo test --release`: 79 unit tests, 4 HTTP tests | |

The 9 top-10 misses against Solr are the same 9 for Python and Rust (BOOK_SEARCH `language:eng`, `subject:"lending library"` and `the`, 9 of 10 keys each), so they are minisolr semantics, not port bugs.

**Is the RSS rise real growth?** No. Step 2 saw RSS go from 10.6 MiB (step 1) to 17.7-22.8 MiB. `heap` shows the live malloc heap at 1.98 MB just after load and 1.99 MB after 1,300 and after 6,500 queries, with the same 20,445 live allocations after 1,300 and 6,500: nothing accumulates. RSS climbs from about 10 MiB at load to about 21 MiB in the first few hundred queries and then stays flat or drops. `vmmap` puts the difference in freed `MALLOC_MEDIUM` regions that macOS keeps resident (4-5 MiB in "empty" medium regions) and in the tiny/nano zones: allocator slack from per-query buffers of 32 KiB and up (the BOOK_SEARCH shapes with many `qf`/`pf` fields and the block join), which `footprint` (dirty memory, 9-14 MiB) does not count. Scoring every hit when a function is present adds one vector per query the size of the index (447 entries here). On Linux in a container the allocator (glibc or musl) differs, so the number to trust is the one step 9 will measure there.

**Recommendation: go, but reorder the remaining steps and decide on the maintenance cost first.** The technical risk is gone: on everything that is answerable, the Rust server returns exactly what Python minisolr returns, about 4x less dirty memory than Python (9-14 against about 55 MiB `footprint`, host to host) and 2.5x (p50) to 6x (p95) lower latency, with no new crates since step 1. The 20 MiB RSS bar is missed by a few MiB on macOS, but only through reusable allocator slack; `footprint` is well under it. What argues against continuing is not risk but cost and value:

- **Cost.** About 8-12 more days *(est.)*: facets (2), updates, persistence and nested documents (3-4), remaining analysis (1-2), highlighting and spellcheck (1), packaging (1-2). Steps 1-3 needed about as much code per feature as expected; Rust is about 2x Python's lines for the same behaviour.
- **Value.** Python minisolr already saves about 80% of Solr's memory; Rust saves another 25-30 MiB per development stack. It is worth it only if that matters (small laptops, CI runners, many parallel stacks).
- **Maintenance.** Two implementations must then agree. The fixtures and `tools/compare_python.py` make that checkable, but every Python minisolr change needs a Rust twin until one of them is retired.
- **Order.** Do step 6 (updates and persistence) before facets: without `/update` the Rust server cannot run in the dev stack at all (the `solr-updater` writes to it), so it is the step that turns the port into something usable, and the riskiest one left. Then facets, packaging (to get the Linux memory number early), and analysis, highlighting and spellcheck last.

If the extra 25-30 MiB per stack is not worth 2 more weeks and a second implementation, stop here: the result (exact parity on 261 of 267 requests at a third of the memory) is a solid answer to the original question.

## Decisions to make before step 1

- **Where it lives.** A separate repo (as now), or a `rust/` directory inside minisolr so the fixtures, harness and docs are shared. Sharing is simpler; a separate repo keeps minisolr pure Python.
- **Dependency budget.** The prototype uses four crates (125 in the tree). Step 1 adds an XML parser. Anything beyond that (a regex engine, an HTTP framework) should be justified; the machine struggles with big builds.
- **Success bar.** Same as Python: `numFound` equal on 267/267, top-10 overlap at or above 90%, probes 66/66, smoke 86/86. Add: memory below 20 MiB and p95 below 10 ms, otherwise the port is not worth keeping.
- **Go/no-go checkpoint.** After step 3 (parser, edismax and boost), rerun the replay. If the answerable share is not near 100% or the memory number has drifted up, stop. (Done: see the checkpoint above.)

## Build budget

Keep builds small: `nice -n 19`, `CARGO_BUILD_JOBS=2`, `cargo check` and `cargo clippy` while iterating, one release build per measurement, `CARGO_HOME` and `CARGO_TARGET_DIR` outside the repo and deleted afterwards. After step 1 the target directory is about 0.8 GB with debug and release builds.

## Tools

`tools/replay.py` (replay against golden, the main progress meter), `tools/compare_python.py` (request-by-request comparison with Python minisolr, with its boosts on or neutralized), `tools/bench.py` (RSS, `footprint`, latency) and `tools/dump_tokens.py` (Python tokens for `parity`); usage is in each file's header and in `README.md`.
