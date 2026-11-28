# What a developer gains from minisolr

Measured on 2026-11-27 with Open Library's own dev stack, once with real Solr (`solr:10.1.0`, from `compose.yaml` + `compose.override.yaml`, heap 128m) and once with minisolr (the same plus `compose.ghcr.yaml`, image `ghcr.io/raybb/minisolr:latest`). Nothing else differs between the two.

## Method

- Machine: Apple Silicon Mac, 16 GB RAM, OrbStack (Docker VM limit 7.8 GiB), arm64 images only. The Mac was in normal use: the 1-minute load average was 8 to 22 at the start of the runs (listed per run below). The plan was to wait for it to drop below 3; it did not within five minutes, so the runs were done under that load.
- N = 3 per engine, alternating A, B, A, B, A, B (A = real Solr, B = minisolr). Each run began with `docker compose down -v`, then `up -d`, with images already present locally. Each run used its own project name and non-default host ports. No other compose stack was running during the runs.
- Timed from the start of `docker compose up -d`: until the `solr` container reports healthy (its compose healthcheck), and until `http://localhost:8881/` answers 200. `up -d` itself waits for solr because other services depend on it being healthy, so the two numbers are close.
- Reindex: `docker compose run --rm home make reindex-solr`, 446 documents in both engines (`numFound` from `/select?q=*:*&rows=0`). The Makefile first downloads `osp_totals.db` from archive.org (127 to 200 s here, pure network). That download was run separately beforehand and is excluded from the reindex time.
- Search: time until the first `/search.json?q=twain` through the web port returned results after the reindex, then the median of 20 following identical requests (client-side wall time, includes web.py and FastAPI).
- Memory: `docker stats --no-stream`, summed over the 9 long-running containers (db, memcached, infobase, web, fast_web, covers, solr-updater, mockservices, solr; the one-off `home` container is not counted). "Idle" was taken 60 s after the reindex and the 20 search requests; "after 200" after 200 mixed `/search.json` requests (twain, subject:fiction, title:alice, alice, history, with and without `sort=new`; all 200 returned 200 in every run).
- Disk: `docker image inspect` sizes, and `du` of the minisolr data volume.

## Results

Median (min to max) over 3 runs.

| Metric | Real Solr | minisolr | Difference |
|---|---|---|---|
| solr healthy after `up -d` | 40.7 s (31.2 to 95.2) | 29.5 s (24.7 to 57.8) | about 11 s faster, but ranges overlap |
| site answers 200 | 44.7 s (36.0 to 107.7) | 42.4 s (32.8 to 68.5) | no clear difference |
| `make reindex-solr` (446 docs) | 38.5 s (27.3 to 53.9) | 30.1 s (21.3 to 87.5) | no clear difference |
| first search after reindex | 0.6 s (0.5 to 1.6) | 0.5 s (0.4 to 1.3) | none |
| median of 20 `/search.json?q=twain` | 24.8 ms (10.8 to 30.2) | 12.3 ms (11.4 to 25.7) | not significant |
| solr container memory, idle | 405 MiB (400 to 425) | 6.7 MiB (6.3 to 7.2) | about 400 MiB less (60x) |
| solr container memory, after 200 searches | 414 MiB (411 to 430) | 5.6 MiB (5.0 to 7.2) | about 408 MiB less |
| whole stack memory, idle | 1289 MiB (1284 to 1307) | 881 MiB (878 to 883) | about 408 MiB less (32%) |
| whole stack memory, after 200 searches | 1298 MiB (1295 to 1313) | 882 MiB (881 to 888) | about 416 MiB less (32%) |
| 200 mixed searches, total wall time | 6.1 s (3.6 to 7.4) | 4.5 s (3.3 to 16.1) | not significant |
| solr image size | 1.09 GB (1,084,586,072 bytes) | 7.7 MB (7,682,734 bytes) | about 1.08 GB less (140x) |
| first pull of the image | not measured | 17.5 s | |
| solr data volume after reindex | not measured (see caveats) | 592 KB | |

Per-run values (run 1, 2, 3):

- Real Solr: healthy 40.7, 31.2, 95.2 s; reindex 27.3, 53.9, 38.5 s; search median 10.8, 24.8, 30.2 ms; load average at start 8.6, 12.8, 14.6.
- minisolr: healthy 24.7, 57.8, 29.5 s; reindex 87.5, 30.1, 21.3 s; search median 25.7, 12.3, 11.4 ms; load average at start 9.2, 22.2, 16.8.

## Caveats

- Timing is noisy. The host load average was between 8 and 22 (other work on the same Mac), and the spread within one engine (95 s vs 31 s to become healthy; 87 s vs 21 s to reindex) is as large as the difference between engines. Only the memory and disk numbers are stable across runs and clearly separate the two. Read every timing row as "similar, perhaps slightly faster with minisolr", not as a measured speedup.
- The data set is tiny (446 documents). Search and indexing behaviour on a realistic corpus is not tested here.
- Images were already present locally (warm cache). The pull time is only for minisolr (17.5 s, 7.7 MB, over this connection); real Solr's pull was not timed because `solr:10.1.0` is shared with other stacks on the machine and was not removed.
- Only the Solr part differs. The other eight containers (about 875 MiB) are identical, and web.py/FastAPI dominate search latency, so the search timings mostly measure Open Library, not the engine.
- arm64 only (Apple Silicon, OrbStack). Not tested on Intel, Linux or Docker Desktop.
- The idle sample was taken 60 s after the reindex and 20 searches, not 60 s after the site first became healthy. Solr's number depends on the 128m heap set in `compose.override.yaml`.
- Docker VM memory used was not measured; only per-container `docker stats` figures are reported.
- No like-for-like Solr volume size: my measurement of it failed (du ran as a non-root user), and the repeat run was abandoned when another session started an unrelated stack on the machine. For orientation only, the Solr data volume of another, stopped project here (`solr:10.1.0`, unknown document count) was about 1.5 MB. At this data size both volumes are tiny.
- A first attempt at the second real Solr run crashed in the benchmark script after measuring (a bug in the volume check), so the later runs were repeated. The table uses one A and one B run from the first pass and the other two of each from the second.

## Conclusion

- A developer using minisolr saves about 400 MiB of RAM (the Solr container goes from about 405 MiB to about 7 MiB, and the whole dev stack from about 1.29 GB to 0.88 GB) and about 1 GB of disk and image download. Start-up, reindex and search speed look similar to slightly better, but this data does not prove a speed gain.
- On a 16 GB laptop already running the Open Library stack alongside a browser and an editor, a third less memory is worth having and costs one extra compose file; on a larger machine the gain is small in practice.
- That justifies recommending it as an opt-in option for memory-constrained developers, with real Solr remaining the default and the reference for anything that depends on exact Solr behaviour.
