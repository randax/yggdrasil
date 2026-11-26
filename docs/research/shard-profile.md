# Shard profile on a realistic corpus

Research for [#130](https://github.com/randax/yggdrasil/issues/130): where do
time and bytes go in today's Shards? This report indexes 36 public repos (six
per supported language) on the local compose stack. It then measures five
things: Shard sizes, index time per pass, cold and warm Verb latency,
`neighbors` fan-out, and payload bytes per Verb.

Every number is tagged **[measured]** (taken in this run) or **[estimate]**
(derived, with the derivation given). Per-repo tables are in
[`shard-profile/results/summary.md`](shard-profile/results/summary.md). Raw
data and every script are in [`shard-profile/`](shard-profile/).

## Headlines

- **Bytes go to the graph segment, and the graph's bytes go to CALLS
  edges.** The median Shard costs 642 B/edge (range 457–1184) **[measured]**.
  The graph segment is a median 82% of a Shard's bytes, and 95% for large
  repos **[measured]**. CALLS edges are 75% of all edges **[measured]**.
  94% of those CALLS edges sit at call sites that resolve to more than one
  same-named candidate. 84% sit at sites with more than 10 candidates
  **[measured]**. One edge per call site would cut corpus edges from 6.83 M
  to about 2.2 M **[estimate]**.
- **Index time is dominated by the clone, then by `prepare_shard`.** Across
  the corpus, fetch (a full-history bare clone) took 810 s and indexing took
  258 s **[measured]**. Inside indexing, `prepare_shard` (SQLite and tantivy
  builds) is 51% of pass time and `syntactic_pass` is 28% **[measured]**.
  End-to-end index time is a median 61 ms per kLOC **[measured]**, with a
  floor of about 1–2 s per repo (the worker's 2 s idle poll).
- **Cold is whole-segment download.** A cold `node` call costs about 6.2 ms
  per MB of `graph.sqlite` **[measured]**, so large repos pay 3–6 s on first
  touch. Warm p50 latency is 1–15 ms for every Verb in every size class
  **[measured]**.
- **`neighbors` on a hub returns a payload no agent can use.** The default
  page (limit 100) from etcd's top symbol is 1.44 MB, about 360 k tokens
  **[measured]**. That page has 4,356 edges but only 395 distinct
  (src, dst, kind) triples, because every call site is its own edge
  **[measured]**. Node-id strings take 54% of its bytes, and the repeated
  repo qualifier alone takes 14% **[measured]**.
- **Two behaviours worth a ticket** (details under Findings): the per-node
  1,000-edge cap is applied *before* the direction filter, so
  `direction: "in"` on a hub silently loses callers. Depth-3 traversals from
  hubs return hundreds of nodes and zero edges.

## Setup

| | |
|---|---|
| Machine | Apple M1 Pro (10 cores), 32 GiB RAM, internal Apple SSD (APFS, 98% full during the run), macOS 27.2.1 |
| Docker | 29.9.3 (VM: 10 CPUs, 11.7 GiB), Postgres 17, RustFS 1.0.4 (compose-pinned digest) |
| Yggdrasil | `c4bec0c0b806f4b8f479073fa370838ff51bf241` (`main`), `cargo build --release`, toolchain 1.96.0 |
| Tools | git 2.54.0, tokei 15.0.0, sqlite3 3.54.0 (dbstat), Python 3.15.2 (stdlib only, `python3 -I`) |
| Date | 2026-11-26, about 45 min of measurement wall time |

### Corpus

There are 36 repos, listed in [`corpus.tsv`](shard-profile/corpus.tsv) with
the pinned commit each one was indexed at. LOC is tokei code lines (no
comments or blanks) in the six indexed languages, over the extracted tree.
The size classes used below are **S** < 10 k LOC, **M** 10 k–300 k, and
**L** ≥ 300 k.

| Language | S | M | L |
|---|---|---|---|
| Go | google/uuid | spf13/cobra, gin-gonic/gin, caddyserver/caddy, etcd-io/etcd | prometheus/prometheus |
| Java | square/javapoet | google/gson, apache/commons-lang, mockito/mockito, junit-team/junit5 | google/guava |
| JavaScript | — | expressjs/express, axios/axios, lodash/lodash, chartjs/Chart.js | eslint/eslint, webpack/webpack |
| TypeScript | pmndrs/zustand | sindresorhus/ky, colinhacks/zod, nestjs/nest, vitejs/vite | microsoft/playwright |
| Python | psf/requests | pallets/flask, encode/httpx, pydantic/pydantic, scrapy/scrapy | django/django |
| Rust | dtolnay/anyhow, BurntSushi/walkdir | BurntSushi/ripgrep, clap-rs/clap, tokio-rs/tokio | rust-lang/rust-analyzer |

The class counts are 6 S, 23 M, and 7 L. The largest repo is webpack at
715 k LOC, so no repo is over 1 M LOC. The indexer parses every supported
language in a tree, not only the primary one. Prometheus's TypeScript UI, for
example, is in its Shard and in its LOC.

## Method

1. **Isolated stack.** `docker compose -p yg-profile` on ports 25432, 29000
   and 29001, with the server on 27311. Caches live in the gitignored
   `data/shard-profile/`. The dev stack and its volumes were never touched.
   `YG_POLL_INTERVAL` and `YG_DISCOVERY_INTERVAL` were set to 10 days, so
   nothing re-indexed or spent GitHub API budget mid-run. No token was
   needed.
2. **End to end** ([`index_e2e.py`](shard-profile/index_e2e.py)). The script
   runs `yg admin repo add` for one repo at a time and waits for its Shard
   before adding the next, so the timings include no queue wait. Times come
   from Postgres: `jobs.created_at` and `finished_at` per kind, and
   `shards.published_at`.
3. **Per pass** ([`harness/`](shard-profile/harness/src/main.rs), driven by
   [`harness.py`](shard-profile/harness.py)). A scratch binary outside the
   workspace replays the worker's `prepare` stages on the server's own bare
   mirror and commit. It runs under `/usr/bin/time -l` to capture peak RSS,
   while the server is idle. It publishes to a local-filesystem
   `object_store` and picks Verb arguments from the local segment
   ([`hubs.py`](shard-profile/hubs.py)).
4. **Sizes** ([`shard_sizes.py`](shard-profile/shard_sizes.py)). Sizes come
   from RustFS ListObjectsV2, which is what object storage actually holds.
   [`dbstat.sh`](shard-profile/dbstat.sh) breaks down four kept segments.
   [`ambiguity.py`](shard-profile/ambiguity.py) downloads every
   `graph.sqlite` and measures CALLS fan-out per call site.
5. **Verbs** ([`verbs_bench.py`](shard-profile/verbs_bench.py)). There is one
   pass per Verb. Each pass restarts `yg serve --role=all` with an empty
   Shard cache dir and then walks the repos one by one. The first call on a
   repo is its **cold** sample. Every call is then repeated 20 times over one
   keep-alive connection for the **warm** sample, and the latency is the
   client wall time. The Prometheus counters confirm the cold samples are
   real: each pass recorded exactly 36 Shard-cache misses (one per repo),
   `search` touched only `fts`, and the other Verbs touched only `graph`.
   The `neighbors` pass also follows `next_cursor` to the end for each
   variant (the **fan-out** phase).
6. **Payload anatomy.** [`anatomy.py`](shard-profile/anatomy.py) and
   [`edge_page.py`](shard-profile/edge_page.py) dissect one etcd response per
   Verb. Tokens are approximated as bytes ÷ 4.

## 1. Shard sizes

All Shard sizes are **[measured]** from object storage. The corpus totals
5.00 GB.

| Class | Repos | LOC median | Shard MB, median (range) | graph MB | FTS MB | Graph share | B/edge |
|---|---|---|---|---|---|---|---|
| S | 6 | 6.6 k | 2.2 (0.6–7.1) | 1.4 | 0.5 | 72% (56–82) | 644 (460–818) |
| M | 23 | 66 k | 18.5 (2.4–593) | 14.6 | 5.0 | 83% (45–98) | 619 (457–1184) |
| L | 7 | 472 k | 513 (36–1051) | 492 | 29.1 | 95% (65–97) | 671 (523–927) |

| Measure | Median (range) |
|---|---|
| Total B/edge | 642 (457–1184) |
| Graph B/node | 1,386 |
| Total B/LOC | 402 (76–4254) |
| Edges per LOC | 0.66 (0.11–3.6) |
| FTS bytes ÷ tree bytes | 1.01 (0.27–1.95) |

The biggest Shards are webpack (1.05 GB), guava (712 MB), rust-analyzer
(677 MB), junit5 (593 MB) and prometheus (513 MB) **[measured]**. junit5
spends 4,254 B per LOC because its 504 k edges come from only 139 k LOC.

**Edge mix across the corpus [measured].** CALLS are 76.0% of edges,
TOUCHES 10.8%, DEFINES 5.6%, AUTHORED 4.6%, IMPORTS 2.8%, and
IMPLEMENTS/EXTENDS under 0.1%. The history layer (Commit and Contributor
nodes, AUTHORED and TOUCHES edges) is 41% of nodes but only 15% of edges.

**CALLS fan-out ([`results/ambiguity.tsv`](shard-profile/results/ambiguity.tsv)) [measured].**

| Measure | Value |
|---|---|
| CALLS edges | 5.14 M |
| Distinct call sites (src, location) | 518 k |
| Share of CALLS edges at sites with more than one candidate | 94% |
| Share of CALLS edges at sites with more than 10 candidates | 84% |
| Worst single site | 1,379 candidates (webpack) |

The fan-out tracks language. Go, Java and Rust repos have a median of
5.0–5.7 CALLS edges per site, against 1.25–1.4 for JS, TS and Python. Go,
Java and Rust method calls (`x.Context()`, `t.Errorf()`) fan out to every
same-named method in the repo, per ADR 0006's "one edge per candidate"
policy. That is why Go, Java and Rust reach 1.1–1.3 edges per LOC, against
0.3–0.7 for JS, TS and Python.

**dbstat breakdown [measured].** For gin, etcd, pydantic and django
([`results/dbstat.tsv`](shard-profile/results/dbstat.tsv)), the `edges`
table and its four indexes hold 91–97% of `graph.sqlite`.

| Repo | `edges` | `edges_unique_with_location` | `edges_src` | `edges_dst` | `edges_unique_without_location` | `nodes` + autoindex |
|---|---|---|---|---|---|---|
| gin | 32% | 23% | 14% | 12% | 9% | 9% |
| etcd | 34% | 33% | 14% | 13% | 3% | 3% |
| pydantic | 31% | 24% | 14% | 12% | 8% | 10% |
| django | 30% | 16% | 14% | 13% | 15% | 12% |

Each edge stores about 122 bytes of text before index overhead (median):
two full-path endpoint ids, kind and provenance as text, and a
`path:line:col` location that repeats the source path. The unique index and
the src and dst indexes then store the ids again. This agrees with the
storage-engines research (460–530 B/edge on two cargo crates, without
history). That research's interned layout measured about 9× smaller.

## 2. Index time

All times in this section are **[measured]**. E2E means from the fetch job
finishing to the Shard being published (claim, extract, history, parse,
build, upload, settle). Pass times come from the harness.

| Class | Fetch s, median (range) | E2E index s | E2E ms/kLOC | Harness peak RSS MB |
|---|---|---|---|---|
| S | 2.0 (1.1–3.7) | 1.9 (1.1–2.3) | 373 (183–670) | 57 |
| M | 5.9 (1.9–94) | 3.3 (0.9–14.8) | 59 (25–137) | 163 (62–1169) |
| L | 46 (11–218) | 15.2 (4.6–52.5) | 39 (10–73) | 1307 (272–1873) |

Pass totals over the whole corpus (harness):

| Pass | Corpus total s | Share |
|---|---|---|
| `extract_tree` (`git archive \| tar`) | 19.4 | 12% |
| `history_walk` (`extract_history`: git log and parse) | 11.6 | 7% |
| `syntactic_pass` | 44.4 | 28% |
| `merge_history` | 0.1 | 0% |
| `prepare_shard` | 80.9 | 51% |
| — of which FTS build (timed alone) | 26.6 | |
| — of which graph build (prepare minus FTS) **[estimate]** | 54.2 | |
| write to local FS | 2.6 | 2% |

- **The slowest pass is `prepare_shard`** in 28 of 36 repos and
  `syntactic_pass` in the other 8. Graph build is the largest piece in big
  Go, Java and Rust repos: 9.6 s of rust-analyzer's 10.7 s, and 12.3 s of
  webpack's 14.9 s. That matches where the edges are.
- **FTS build has a fixed cost of about 0.2 s** (tantivy writer setup and
  commit), so small repos are dominated by FTS. Below about 10 k LOC, the
  2 s worker idle poll plus this floor set E2E time, not LOC.
- **The clone is the longest step.** guava took 218 s to fetch and 21 s to
  index; pydantic 94 s and 6 s; nestjs 78 s and 6.5 s. Full-history bare
  clones total 4.7 GB on disk for this corpus.
- **Peak indexing memory is about 3.3× the graph segment size**
  **[estimate]**: the median ratio of harness peak RSS to `graph.sqlite`
  bytes for segments over 50 MB. webpack peaked at 1.87 GB.
  `PreparedShard` holds both segments in memory, and the graph is built
  through rusqlite first.
- **E2E time exceeds the harness pipeline by a median of 1.6 s.** That is
  the claim poll (≤ 2 s) plus upload to RustFS, which measured about
  120 MB/s PUT and 155 MB/s GET on this host. The large-repo gaps of
  13–15 s for webpack and rust-analyzer are mostly upload of 0.7–1 GB
  **[estimate]**. tokio's 12 s gap is **unexplained**: its Shard is only
  106 MB. The worker emits no per-stage timing, so this run cannot attribute
  it.

## 3. Verb latency, cold and warm

All latencies are **[measured]**, client wall time over HTTP/1.1 keep-alive.
Cold n is one sample per repo; warm n is 20 repetitions of each call in the
class.

| Verb | Class | Cold p50 ms | Cold max ms | Warm n | Warm p50 ms | Warm p95 ms | Warm max ms |
|---|---|---|---|---|---|---|---|
| node | S | 18 | 43 | 480 | 0.95 | 1.5 | 2.9 |
| node | M | 134 | 3,573 | 1,840 | 1.25 | 3.2 | 10.6 |
| node | L | 3,014 | 6,019 | 560 | 2.33 | 9.2 | 10.9 |
| neighbors | S | 36 | 57 | 3,480 | 5.5 | 40 | 87 |
| neighbors | M | 172 | 3,641 | 13,060 | 7.2 | 71 | 836 |
| neighbors | L | 3,013 | 6,182 | 4,060 | 15.4 | 190 | 519 |
| search | S | 17 | 29 | 480 | 4.4 | 14.2 | 32 |
| search | M | 73 | 176 | 1,840 | 6.7 | 17.2 | 81 |
| search | L | 281 | 476 | 560 | 10.8 | 15.0 | 28 |
| history | S | 21 | 38 | 360 | 1.8 | 3.4 | 16 |
| history | M | 183 | 4,356 | 1,380 | 3.1 | 10.9 | 24 |
| history | L | 3,085 | 6,305 | 420 | 6.2 | 14.0 | 18 |

- **Cold latency scales with the graph segment, not with the query.** The
  first `node`, `neighbors` or `history` call on a repo downloads the whole
  `graph.sqlite` and verifies its sha256. That costs 6.0–7.0 ms per MB for
  segments over 50 MB, so webpack's 1 GB segment takes 6 s. `search` only
  fetches `fts.tar` (2–50× smaller than the graph segment in L repos), so its cold p50 is 281 ms even for L
  repos.
- **Warm `neighbors` p95 is driven by hub payloads, not lookups.** The same
  hub calls that return 1–3 MB take 150–500 ms.
- **Cross-repo `search`** (no `repos` filter, all 36 Shards, warm) takes
  57–82 ms p50. The cursor in that response is 6.0 KB of 12.8 KB, because it
  carries every target repo and its revision.
- The per-call breakdown (44 call shapes) is in
  [`summary.md`](shard-profile/results/summary.md#per-call).

## 4. `neighbors` fan-out

Fan-out data is **[measured]** in
[`results/fanout.tsv`](shard-profile/results/fanout.tsv). It covers 29
variants per repo: the four hubs (top node overall, top Symbol, top File,
top Package), seven shapes each (d1, d1 with limit 1000, out, in, CALLS
only, d2, and d3 with limit 1000), and one median-symbol d1. Each variant
was followed through `next_cursor` to the end.

| Worst case | Repo, call | Result |
|---|---|---|
| Largest single page | etcd, top symbol, d1 limit 1000 | 3.23 MB, 10,000 edges (page edge cap), about 800 k tokens |
| Largest default page (limit 100) | etcd, top symbol, d1 | 1.44 MB, 100 nodes, 4,356 edges; warm p50 171 ms |
| Most bytes over all pages | prometheus, top node, d1 | 7.2 MB over 5 pages |
| Most bytes over all pages | caddy, top symbol, d1 | 7.0 MB over 3 pages, 268 nodes, 20,460 edges |
| Most pages | mockito, top Package, d2 | 63 pages, 6,270 nodes, 1.7 s |
| Slowest full walk | pydantic, top Package, d3 limit 1000 | 45 pages, 6.4 s |

- **Edges dominate every hub page.** In the etcd default page, edges are
  1.47 MB and nodes 19 KB. The page carries 4,356 edges for 100 nodes, with
  up to 28 parallel CALLS edges per node pair (one per call site). It also
  includes every edge among already-seen nodes, not only edges to the
  origin: only 2,717 of the 4,356 touch the origin.
- **The page size changes the answer.** etcd's top symbol at d1 returns
  13,015 edges over three pages at limit 100. At limit 1000 it returns
  exactly 10,000 (`MAX_NEIGHBORS_EDGES_PER_PAGE`), with `truncated: true`.
- **Depth beyond 1 truncates almost immediately on hubs.** See Findings 2.

## 5. Payload bytes per Verb

Payload sizes are **[measured]** response bytes, with tokens as
bytes ÷ 4 **[estimate]**. Medians are across repos, at default limits unless
named.

| Call | Median bytes | Max bytes | Median tokens |
|---|---|---|---|
| `node` (symbol, file or hub) | 215–337 | 599 | 53–84 |
| `search` (20 hits, one repo) | 3.7–6.3 K | 9.6 K | 930–1,580 |
| `search` limit 100 | 22 K | 34 K | 5.5 K |
| `history` hot file (50 commits) | 20 K | 25 K | 5.1 K |
| `history` hot file, limit 1000 | 172 K | 471 K | 43 K |
| `neighbors` median symbol d1 | 0.9 K | 27 K | 234 |
| `neighbors` hub d1 (limit 100) | 36–78 K | 1.44 M | 9–20 K |
| `neighbors` hub d1, limit 1000 | 193–357 K | 3.5 M | 48–89 K |
| `neighbors` hub d3, limit 1000 | 216–420 K | 1.8 M | 54–105 K |

What dominates one etcd example per Verb **[measured]**
([`results/anatomy_examples.json`](shard-profile/results/anatomy_examples.json)):

| Call | Bytes | Node-id strings | Repo qualifier inside the ids | JSON keys | Other strings | Cursor |
|---|---|---|---|---|---|---|
| `node` top symbol | 398 | 22% | 6% | 43% | 23% | 0% |
| `search` "init" | 3,830 | 31% | 12% | 22% | 34% | 8% |
| `history` hot file | 20,483 | 32% | 11% | 19% | 43% (subjects, names, emails) | 2% |
| `neighbors` top symbol d1 | 1,439,061 | 54% | 14% | 17% | 21% (kind, provenance, location) | 0% |

Every id spells out `kind:github.com/<owner>/<repo>:<path>#<name>`, and every
edge repeats both endpoints in full. So the repo qualifier
(`github.com/etcd-io/etcd`, 23 bytes) recurs twice per edge, and the source
path recurs a third time in `location`. `history` also repeats the author's
email inside the contributor id.

## Findings

1. **The per-node edge cap is applied before the direction filter
   (likely bug).** `incident_edges` (`crates/yg-verbs/src/lib.rs`) merges a
   node's out- and in-edges by rowid and truncates to
   `MAX_NEIGHBORS_EDGES_PER_NODE` (1,000). `follows` applies `direction`
   only after that. A function with many outgoing ambiguous CALLS therefore
   loses its callers.

   Repro on this corpus: `POST /v1/verbs/neighbors
   {"id": "sym:github.com/etcd-io/etcd:server/etcdserver/api/rafthttp/stream.go#streamWriter.run", "direction": "in"}`
   returns 8 nodes and `truncated: true`. The Shard holds 12 in-edges from
   12 distinct callers (`select count(*) from edges where dst = 'sym:server/etcdserver/api/rafthttp/stream.go#streamWriter.run'`)
   next to 5,286 out-edges. The truncation is disclosed, but no cursor can
   recover the missing callers.
2. **Depth-2 and depth-3 traversals from hubs return nodes with no edges.**
   Once discovery spends `MAX_TRAVERSAL_EDGES` (10,000), edge assembly is
   skipped by design. etcd's top symbol at depth 3 with limit 1000 returns
   855 nodes, 0 edges and `truncated: true`. Depth 3 from a hub is
   therefore a node list, not a subgraph.
3. **Ambiguous name-based CALLS are the main cost.** They drive Shard size,
   graph build time, peak indexing memory, cold latency, and `neighbors`
   payload. Ways to reduce them, none measured here: one edge per call site
   (about 68% fewer edges **[estimate]**), a candidate cap per site, and
   interned ids. These are inputs for the storage and agent-efficiency
   tickets.
4. **No errors.** Every one of about 37 k Verb HTTP calls (31 k recorded
   samples plus fan-out pages) returned 200, including
   6 s cold fetches under the default 30 s request timeout. A segment of
   about 4–5 GB would hit that timeout on this host **[estimate]**. Five LRU
   capacity evictions occurred under the 4 GiB cache cap, each after its
   repo had been measured.
5. **The Shard cache stores FTS twice.** It keeps both `fts.tar` and the
   unpacked `.fts` directory, so the FTS segment takes about 2× its size in
   cache disk **[measured]** (directory listing of the cache).

## Caveats

- This is one machine, one run, and a localhost RustFS with no network
  latency. Cold times against real S3 will be higher. Warm numbers include
  client overhead of about 0.2 ms.
- `extract_tree` and the separate `git log` timing ran first on each mirror,
  so they include cold page-cache reads. `history_walk` ran second, which is
  why the `git log` time alone (16.9 s corpus total) exceeds the full
  history walk (11.6 s).
- The graph-build split is an **[estimate]**: `prepare_shard` minus a
  separate `build_fts` run over a copy of the docs. For tiny repos it goes
  slightly negative from noise.
- Harness numbers come from a separate process, not the server's worker.
  E2E numbers come from Postgres timestamps, which include the worker's 2 s
  idle poll.
- The cold sample is the first call of each Verb pass on a repo. A pass
  restarts the server with an empty cache dir, but the OS page cache and
  RustFS are warm.
- LOC is tokei code lines for the six indexed languages only. Large
  generated or vendored test fixtures, such as webpack's test cases, count
  as code.

## Deviations from the ticket's method

- **The corpus is 36 repos.** The JavaScript slots ended up as 4 M and 2 L
  with no S repo. Rust has two S repos. No repo is over 1 M LOC.
- **The harness re-implements two private stages.** `extract_tree` is
  private, so the harness runs the same `git archive <sha>^{tree} | tar -x`
  pipeline itself. `merge_history` is `pub(crate)`, so its 15-line body is
  copied verbatim. `extract_history_for_commit` is `pub(crate)`, so the
  public `extract_history` wrapper is timed instead. `git log` is also timed
  alone, with the worker's exact arguments.
- **The harness writes Shards through `yg_shard::publish_shard`** into an
  `object_store` `LocalFileSystem` (the segment bytes are private), not
  `write_shard`. Upload cost to RustFS is a separate curl PUT/GET throughput
  probe, not timed per repo.
- **Shard sizes come from object storage**, not the harness output. They
  agree to within a few KB: tantivy output is not byte-deterministic.
- **Most harness graph segments were deleted after analysis** for disk
  space. Only gin, etcd, pydantic and django were kept for dbstat. The CALLS
  analysis re-downloads each segment from RustFS.
- **The Verbs ran against `--role=all`**, not a split API process (the
  worker was idle). `YG_SHARD_CACHE_MAX_BYTES` was capped at 4 GiB for disk
  space.
- **No GitHub token was needed.** Public clones and a 10-day poll interval
  stayed within anonymous limits.

## Rerun

From a checkout with Docker running (needs about 15 GB free disk, tokei and
sqlite3):

```sh
cd docs/research/shard-profile
./all.sh --teardown   # builds, starts yg-profile stack, runs every step, tears down
```

`all.sh` writes to a fresh `results-<timestamp>/` dir. Steps can also be run
one at a time with `./run.sh <script>.py`. `index_e2e.py` and `harness.py`
are resumable and skip repos already recorded in their results dir. To
change the corpus, edit `corpus.in.tsv`. Override ports with
`YG_POSTGRES_PORT`, `YG_RUSTFS_PORT` and `YG_SERVER_PORT`, and warm
repetitions with `WARM_REPS`. Teardown is
`docker compose -p yg-profile down -v` plus removing `data/shard-profile/`.
