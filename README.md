# minisolr-rs

A throwaway Rust prototype of [minisolr](../minisolr)'s `/select`, built to answer two questions: how much RAM does a Rust + Tantivy version really use, and can the Solr analyzer chains be reproduced token for token. It is not a port.

## Run

```bash
cargo run --release -- serve /path/to/docs.jsonl 127.0.0.1:8983
curl 'localhost:8983/solr/openlibrary/select?q=author_name:carroll&fq=type:work&fl=key'
cargo run --release -- parity tokens.jsonl   # compare analyzer output with Python's (see below)
cargo test --release
```

The field table in `src/store.rs` is hard-coded (38 fields); the schema XML is not parsed.

## What it supports

`/solr/openlibrary/select` (GET and POST form) and `/solr/openlibrary/admin/ping`. Plain `q` terms, quoted phrases, `field:term` clauses joined by AND, a dismax-like match over a `qf` list (`defType=edismax` or a whole-query `{!edismax qf=... v=$param}` wrapper), `fq` of `field:value`, `rows`, `start`, `fl`, ordering by score or by one numeric field. Boosts, `pf`, `bf`, `boost` are parsed and ignored, so ranking is plain BM25. Highlighting parameters are ignored. Anything else (ranges, wildcards, OR/NOT, facets, function sorts, `{!terms}`, `{!parent}`, subqueries, updates) returns an empty result with a `warning` in `responseHeader` and a line on stderr.

## Analyzers

`text_en_splitting` (whitespace, ICU-style folding, word delimiter with catenation, lowercase, protected words, Porter), `text_international` (standard tokenizer, folding) and `text_general` (standard tokenizer, lowercase). Not done: query-time synonyms; the stop lists are empty in the shipped config. `rust-stemmers` only ships Snowball English (Porter2), which differs from the original Porter that Solr and the Python code use (`odyssey` gives `odyssei`, `one` gives `on`), so `src/porter.rs` is a hand-written Porter (about 220 lines).

## Measurements (Apple Silicon Mac, host, no Docker, machine under swap pressure)

Same 155-request subset (1,300 queries), host to host. macOS compresses and swaps pages under pressure, so RSS jumps around; `footprint` is steadier.

| | Rust | Python minisolr |
|---|---|---|
| RSS just after load / first query | 14.6 MiB (27.8 on another run) | 42.4 MiB before first query, 79.5 right after index build |
| `footprint` idle | about 12 MiB | 55 MiB |
| RSS / footprint after 1,300 queries | 11.1 / 14 MiB | 48.7 / 56 MiB |
| latency p50 / p95 | 1.0-1.2 / 2.4-2.9 ms | 3.7 / 19.5 ms |

Stripped release binary 4.0 MiB; 125 crates; first release build 95 s with 2 jobs (largest compiler process 837 MiB RSS).

Correctness on the captured Open Library requests: 155 of 267 are answerable; `numFound` equals real Solr's on all 155 and the top 10 is the same set on 142 (Python minisolr: 147). Misses are queries with more than 10 hits where the ignored `boost` function decides the order.

## Limits

Single-threaded `tiny_http`, no persistence, no updates, no facets. Unicode categories are approximated with `char` methods and `unicode-normalization`, which cannot tell spacing marks (Mc) from nonspacing marks (Mn); that is the only remaining parity gap (some Telugu text).
