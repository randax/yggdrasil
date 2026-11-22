# Storage engines and layouts for an immutable code graph on object storage

Status: research note · Date: 2026-11-23 · Ticket: #125 (map #121) ·
Yggdrasil baseline: commit `c4bec0c` on `main`

> Scope. This note asks which storage engines and on-disk layouts suit a
> read-mostly code graph that is immutable per revision and served from object
> storage through a local cache. Today each Shard is a SQLite graph segment
> plus a tantivy full-text segment (ADR 0004, ADR 0005, RFC 0001 §6). The note
> compares the options on six criteria: traversal latency, Shard size, cold
> fetch, cross-repo traversal without N-Shard fan-out, Rust ecosystem maturity
> and licence. It then lists options for Yggdrasil with their costs and what
> must be measured before choosing. **It does not choose.**
>
> External claims are cited inline to the owning project's docs, source,
> release data or paper. Repository claims cite paths at the baseline commit.
> Numbers marked **[measured]** come from a local experiment described in
> [Appendix A](#appendix-a-measurement-method). Claims marked **[unverified]**
> could not be traced to a primary source.

## 1. Summary

1. **The bottleneck is Shard size and cold fetch, not warm traversal.** Warm
   one-hop lookups on today's SQLite segment take about 10 µs at p50 and
   0.4 ms at p99 **[measured]**, three orders of magnitude inside the PRD's
   300 ms Verb budget ([PRD](../PRD.md) "Verb latency"). The cost is size.
   The current schema stores every edge's endpoints as full string ids, three
   times over (table plus indexes). The result is about **460–530 bytes per
   edge**, or **≈ 280–310 bytes per line of code**, for the M0 syntactic pass
   alone **[measured]**. Extrapolated to the PRD's ~100M LOC, that is
   roughly 28–31 GB of graph segments before the precise pass adds
   references.
2. **Plain SQLite tuning recovers most of the space.** Integer node ids,
   interned paths, a clustered `WITHOUT ROWID` edge table and one reverse
   index shrink the same graph **8.6–9.3×**, to ≈ 54–57 bytes per edge, and
   make warm lookups about 2× faster **[measured]**. The SQLite docs predict
   gains of this kind for composite keys
   ([WITHOUT ROWID](https://www.sqlite.org/withoutrowid.html)).
3. **A bespoke CSR file is another ≈ 2.4× smaller than tuned SQLite.** Raw
   forward and reverse adjacency plus the string table come to ≈ 22–24 bytes
   per edge, uncompressed and memory-mappable **[measured]**. The cost is a
   custom format and query code for every Verb.
4. **Cold fetch today is whole-object download.** The Shard cache GETs the
   whole graph segment and hashes it before first use
   (`crates/yg-shard/src/lib.rs`, `fetch_verify_into`). Partial loading has
   precedents: page-level SQLite VFSs over HTTP range reads (Litestream VFS,
   SQLite CBS, Graft, sql.js-httpvfs), DuckDB's read-only `ATTACH` over HTTP,
   and the Parquet page index. Every B-tree level is a dependent roundtrip.
   S3 GETs cost about 63 ms at p50
   ([turbopuffer architecture](https://turbopuffer.com/docs/architecture)).
   A cold lookup therefore wants the interior pages cached up front, as
   Quickwit's hotcache does for tantivy splits
   ([Quickwit architecture](https://quickwit.io/docs/overview/architecture)).
5. **Kùzu is gone, and no successor is a safe base yet.** Kùzu was archived on
   2025-10-10 ([kuzudb/kuzu](https://github.com/kuzudb/kuzu)), after Apple
   agreed to buy the company
   ([UWaterloo CS news](https://cs.uwaterloo.ca/news/waterloo-based-graph-database-start-up-kuzu-acquired-apple)).
   Of its forks:
   - **LadybugDB** is active: v0.22.3 shipped on 2026-11-07, it is MIT and its
     core is C++ ([LadybugDB/ladybug](https://github.com/LadybugDB/ladybug)).
   - **Bighorn** is archived (last push 2026-08-10)
     ([Kineviz/bighorn](https://github.com/Kineviz/bighorn)).
   - **RyuGraph** has not released since 2025-12-06
     ([predictable-labs/ryugraph](https://github.com/predictable-labs/ryugraph)).
6. **DuckPGQ is still a research project.** Its README says it is "a research
   project and a work in progress"
   ([cwida/duckpgq-extension](https://github.com/cwida/duckpgq-extension)).
   DuckDB itself is mature (v2.0.1, 2026-11-20) and range-reads both remote
   Parquet files and read-only remote databases
   ([DuckDB httpfs](https://duckdb.org/docs/2.0/core_extensions/httpfs/https.html)).
7. **Columnar formats now do fast random access, at a dependency cost.**
   - **Lance** (Apache-2.0, Rust core) claims "100x faster than Parquet …
     for random access" ([lance-format/lance](https://github.com/lance-format/lance)).
     The Lance paper shows that Parquet itself reaches "over 60x better
     random access" once configured with small pages and a page index
     ([arXiv 2504.15247](https://arxiv.org/abs/2504.15247)).
   - **Vortex** (Apache-2.0, Linux Foundation) has kept its file format stable
     since 0.36.0 ([vortex-data/vortex](https://github.com/vortex-data/vortex)).
   - **The cost:** Lance pulls in DataFusion (69 normal dependencies on
     crates.io).
8. **The cross-repo question is separate from the Shard format.** Three
   patterns avoid N-Shard fan-out:
   - **A cross-repo edge index in Postgres**, which ADR 0005 already plans.
   - **Package-level routing**, as in Sourcegraph's
     `lsif_packages`/`lsif_references` tables
     ([schema](https://raw.githubusercontent.com/sourcegraph/sourcegraph-public-snapshot/main/internal/database/schema.md)).
   - **A global immutable symbol index segment** on object storage, such as an
     FST map from global symbol to postings.

   All three need a globally stable symbol key. SCIP's symbol grammar
   (`<scheme> <manager> <package> <version> <descriptor>+`) provides one
   ([scip.proto](https://github.com/scip-code/scip/blob/main/scip.proto)),
   and RFC 0001 §5 already wants a version-free form of it.

## 2. Today's Shard, as built and read

### 2.1 Layout and access pattern

- **Artifact.** The artifact is
  `shards/<repo-id>/<revision>/{manifest.json, graph.sqlite, fts.tar}`. Every
  put is create-only, and the manifest is written last
  (`crates/yg-shard/src/lib.rs`, `publish_shard`).
- **Schema (v7).** The schema is built in `build_graph_sqlite`, with column
  names in `crates/yg-shard/src/graph_schema.rs`:
  - `nodes(id TEXT PRIMARY KEY, kind TEXT, name TEXT, path TEXT, committed_at INTEGER)`.
  - `edges(src TEXT, dst TEXT, kind TEXT, provenance TEXT, confidence REAL, location TEXT)`,
    an ordinary rowid table.
  - Four indexes on `edges`: two partial `UNIQUE` indexes over
    `(src, dst, kind[, location])`, plus single-column `edges_src` and
    `edges_dst`.
  - Only `journal_mode = OFF` is set. There is no `page_size`, `VACUUM`,
    `WITHOUT ROWID` or integer interning, so the default 4096-byte page
    applies ([PRAGMA page_size](https://www.sqlite.org/pragma.html#pragma_page_size)).
- **Reader.** The reader opens the cached file `SQLITE_OPEN_READ_ONLY`
  (`crates/yg-shard/src/artifact.rs`). It sets neither `immutable=1` nor
  `mmap_size`, and SQLite's default `mmap_size` is zero
  ([mmap](https://www.sqlite.org/mmap.html)).
- **Verbs.** The Verbs that touch the graph are:
  - `node`: a point lookup by string id.
  - `neighbors`: breadth-first search to depth ≤ 3, with at most 1000 edges
    per node and 10 000 per page. It does two index seeks per node (`src = ?`,
    then `dst = ?`), each `ORDER BY rowid LIMIT cap+1`.
  - Edge summaries: `GROUP BY kind, provenance`.
  - `history`: a join over Commit, TOUCHES and AUTHORED.

  All of these are in `crates/yg-verbs/src/lib.rs` (`neighbors`,
  `incident_edges_at`, `edge_summary`, `history`).
- **Cold path.** On a miss, the cache downloads the whole object, streams it
  through SHA-256, renames it into place and only then opens it
  (`fetch_verify_into`).
- **No partial reads yet.** No graph or FTS code issues range reads today.
  Org-wide `search` fans out to every in-scope Shard and merges
  max-normalised BM25 scores (`crates/yg-verbs/src/search/fanout.rs`).
- **No cross-repo edge index yet.** No control-plane migration defines one;
  `crates/yg-control/migrations/` has only `shards.edge_count`.

### 2.2 Measured size

Each Shard was built with the real `yg_index::syntactic_pass` and
`yg_shard::write_shard` over two public Rust crates
([Appendix A](#appendix-a-measurement-method)). The figures are bytes, not
MiB, and both rows are **[measured]**.

| Corpus | LOC (`.rs` lines) | Nodes | Edges | `graph.sqlite` | `fts.tar` | B/edge | B/LOC |
|---|---:|---:|---:|---:|---:|---:|---:|
| tokio 1.54.1 | 148 030 | 7 678 | 91 252 | 42 074 112 | 5 357 056 | 461 | 284 |
| tantivy 0.26.1 | 107 544 | 6 098 | 62 495 | 33 198 080 | 4 275 200 | 531 | 309 |

Where tokio's 40 MiB goes (SQLite `dbstat` **[measured]**):

| B-tree | MiB |
|---|---:|
| `edges` (table) | 13.9 |
| `edges_unique_with_location` | 13.4 |
| `edges_src` | 5.9 |
| `edges_dst` | 5.0 |
| `nodes` and its key index | 1.2 |
| everything else | 0.7 |

The edges are 90% CALLS. The two endpoint ids average 91 bytes per edge, the
location string 31 bytes, and kind and provenance are repeated as text. The
graph segment is **≈ 8× the full-text segment**. zstd‑3 shrinks it 9.8× (to
4.3 MB), which shows how redundant it is. A whole-file compressed blob would
lose mmap and range reads, though.

### 2.3 Layout alternatives on the same graphs

Both tables below are **[measured]**.

| Layout | tokio bytes | tantivy bytes | vs M0 | zstd‑3 (tokio) |
|---|---:|---:|---:|---:|
| M0 schema v7 (today) | 42 074 112 | 33 198 080 | 1× | 4 309 819 |
| Interned SQLite: integer `nid`, paths table, `edges(src,kind,dst,site,conf) WITHOUT ROWID` PK `(src,kind,dst,site)`, one index `(dst,kind,src)`, unique `nodes(id)`, `VACUUM` | 4 902 912 | 3 588 096 | 8.6–9.3× smaller | 836 052 |
| Raw CSR: forward and reverse u32 offsets and targets, u8 kind and confidence columns, u64 packed site, plus newline-joined node ids | 1 703 968 + 355 731 | 1 173 702 + 347 547 | 20–22× smaller | 206 923 + 52 294 |

The interned variant drops the provenance column, which holds only one value
in M0. A real schema would keep it as a one-byte integer. The CSR variant
uses no gap or reference compression. A WebGraph-style encoding would shrink
it further, but by an unmeasured amount.

Warm one-hop lookups (both directions, cap 1001 per side) over 2 000 random
Symbols on a warm OS cache, with `immutable=1` and `mmap_size=256 MiB`. The
harness is Python `sqlite3`, so interpreter overhead is included:

| Layout | p50 | p99 |
|---|---:|---:|
| M0 schema v7 | 9.5 µs | 394.7 µs |
| Interned SQLite | 4.9 µs | 180.8 µs |

Both return the same 47 147 rows. B-tree depth is 3 for every large tree in
both files. Interior pages are 2% of pages: 836 KiB in M0 and 56 KiB in the
interned file.

**Extrapolation [unverified, derived].** Scaling bytes per LOC linearly to the
PRD's "~100M LOC org-wide" gives:

- **M0 schema:** ≈ 28–31 GB.
- **Interned SQLite:** ≈ 3.3 GB.
- **Raw CSR:** ≈ 1.4 GB.

This covers only the syntactic pass on Rust. Other languages, generated code,
and the M1 precise pass's REFERENCES and occurrences (RFC 0001 §5–6) will
change it, and the precise pass will probably raise it by several times.

## 3. Object-storage physics that constrain the design

- **Latency.**
  - turbopuffer publishes S3 GET latency of p50 63 ms, p99 78 ms and p999
    118 ms, and plans on ~100 ms per roundtrip, with cold queries needing 3–4
    roundtrips ([turbopuffer architecture](https://turbopuffer.com/docs/architecture)).
    Its design goal is "a maximum of three roundtrips for sub-second cold
    latency" ([turbopuffer blog](https://turbopuffer.com/blog/turbopuffer)).
  - AWS says requests under 512 KB often see median latencies "in the tens of
    milliseconds range".
  - For single-digit milliseconds, AWS points to S3 Express One Zone
    ([AWS S3 performance design patterns](https://docs.aws.amazon.com/AmazonS3/latest/userguide/optimizing-performance-design-patterns.html)).
- **Throughput comes from parallel byte-range GETs** (same AWS page).
  Yggdrasil's `object_store` 0.13.2 merges ranges less than 1 MiB apart
  (`OBJECT_STORE_COALESCE_DEFAULT = 1024 * 1024`) and runs up to 10 range
  requests in parallel (`OBJECT_STORE_COALESCE_PARALLEL = 10`), both defined
  in `object_store-0.13.2/src/util.rs`. It exposes `get_range` and
  `get_ranges` ([docs](https://docs.rs/object_store/latest/object_store/trait.ObjectStore.html)).
- **Cache tiers.** turbopuffer prices the tiers as DRAM ~100 ns, NVMe ~100 µs
  and S3 ~100 ms. It caches each namespace on NVMe after the first query and
  routes queries for cache locality (architecture page). This is the pattern
  ADR 0005 adopted.
- **Quickwit's hotcache.** Quickwit stores a small `hotcache` file with every
  split. That "makes it possible for Searchers to open a split in less than
  60ms, even on high latency storage". It also keeps split metadata in a
  Postgres metastore
  ([Quickwit architecture](https://quickwit.io/docs/overview/architecture)).
  That metastore role corresponds to Yggdrasil's control plane.
- **Yggdrasil's own cold number** comes from loopback MinIO with a tiny
  fixture: ≈ 22 ms cold and ≈ 11 ms warm for the first search
  ([PRD](../PRD.md) "cold search"). It does not reflect a real store or real
  Shard sizes.

**Implication.** A B-tree point lookup is a chain of dependent page reads.
Over a page-level remote VFS, a cold one-hop Verb on a depth-3 tree would cost
several 60–100 ms roundtrips unless the interior pages are already local. In
the measured files those pages are 56–836 KiB. This is an inference from the
figures above, not a measurement.

## 4. Candidates

### 4.1 SQLite, tuned (the incumbent)

- **Traversal latency.** Warm traversal is already microseconds
  **[measured]**. Specific levers:
  - `WITHOUT ROWID` suits "non-integer or composite (multi-column) PRIMARY
    KEYs". In the best case it uses "about half the amount of disk space and
    can operate nearly twice as fast". It hurts when rows exceed about 1/20 of
    a page, around 200 bytes at 4 KiB
    ([WITHOUT ROWID](https://www.sqlite.org/withoutrowid.html)). Integer-keyed
    edge rows fit easily. Today's text-keyed rows (≈ 150 bytes of payload)
    sit close to that limit.
  - **mmap** avoids copying pages between kernel and user space and shares
    the OS page cache. It is off by default and capped by
    `SQLITE_MAX_MMAP_SIZE`, and an I/O error under mmap is a signal, not an
    error code ([mmap](https://www.sqlite.org/mmap.html)).
  - **`immutable=1`** skips all locking and change detection. That is safe
    here because cached segments are content-addressed and never rewritten
    ([URI parameters](https://www.sqlite.org/uri.html)).
- **Shard size.** About 9× smaller than today, with the schema changes in
  §2.3 **[measured]**.
- **Cold fetch.** Three ways:
  - **Whole file:** today's behaviour. Simple, and it verifies the
    checksummed artifact.
  - **Page-level VFS over range reads.** Several implementations exist:
    - **Litestream VFS** maps each SQLite page read to an S3 `Range` request
      with an LRU cache. It is read-only and has to be loaded as an extension
      ([Fly.io: Litestream VFS](https://fly.io/blog/litestream-vfs/)).
      Litestream is Go and Apache-2.0
      ([benbjohnson/litestream](https://github.com/benbjohnson/litestream)).
    - **SQLite Cloud Backed Storage** (`blockcachevfs`) stores databases as
      immutable blocks (4 MB by default) plus a manifest, with a local LRU block cache.
      It ships Azure and Google Cloud Storage modules and an API for other stores; no S3 module is listed
      ([SQLite CBS](https://www.sqlite.org/cloudsqlite/doc/trunk/www/index.wiki)).
    - **Graft** is a Rust engine for page-granular lazy and partial
      replication over object storage. It self-describes as "**Alpha**
      quality software" and was at v0.2.1 on 2025-12-04
      ([orbitinghail/graft](https://github.com/orbitinghail/graft)).
    - **sql.js-httpvfs** showed the access-pattern tuning a remote VFS needs:
      1 KiB pages, covering indexes and read-ahead heads. It reports 10–20
      GETs (130–270 KiB) for an aggregate query
      ([phiresky blog](https://phiresky.github.io/blog/2021/hosting-sqlite-databases-on-github-pages/)).
      The project's last push was 2024-08 ([repo](https://github.com/phiresky/sql.js-httpvfs)).
  - **Writing our own VFS in Rust.** rusqlite has no safe VFS API in the
    version used. The community crates are `sqlite-plugin` and `sqlite-vfs`
    ([orbitinghail/sqlite-plugin](https://github.com/orbitinghail/sqlite-plugin),
    [rkusa/sqlite-vfs](https://github.com/rkusa/sqlite-vfs)).
- **Cross-repo.** None built in. ATTACH limits are irrelevant at 5000 Shards.
- **Rust maturity.** `rusqlite` 0.43.0 (2026-11-15) is MIT, with about 67M
  recent downloads. Yggdrasil pins 0.39. Turso, the Rust rewrite, is
  file-compatible but "not yet reached 1.0"
  ([tursodatabase/turso](https://github.com/tursodatabase/turso)).
- **Licence.** SQLite is public domain; rusqlite is MIT.

### 4.2 Compact adjacency files (CSR, WebGraph-style)

- **Traversal latency.** A neighbour list is two array reads, and the code
  traverses only what it needs. The cost is that each Verb's query becomes
  Rust code rather than SQL, which ADR 0003's bounded Verbs make feasible.
- **Shard size.** Raw CSR plus the string table is ≈ 22–24 bytes per edge
  **[measured]**. WebGraph adds gap and reference compression, with sequential
  scans needing no offsets file and random access needing an Elias–Fano
  offsets file. Its graphs load memory-mapped by default
  ([webgraph docs](https://docs.rs/webgraph/latest/webgraph/)). webgraph-rs
  handles the Software Heritage graph of about half a trillion arcs
  ([WWW'24 paper record](https://hal.archives-ouvertes.fr/hal-04494627v1)).
  Code graphs have weaker locality than web graphs, so the bits-per-link gain
  is **[unverified]** for this workload.
- **Cold fetch.** A footer or manifest with section offsets gives cheap range
  reads. A one-hop lookup needs about one roundtrip to the offsets, one to the
  targets and one to the attributes. That count is a design estimate, not a
  measurement.
- **Strings.** Ids, names and paths still need a sorted string-to-`nid` map.
  The `fst` crate builds ordered maps of byte strings and leans on memory maps
  for fast range queries ([BurntSushi/fst](https://github.com/BurntSushi/fst));
  tantivy uses the same approach for its term dictionary.
- **Rust maturity.**
  - `webgraph` 0.7.0 (2026-10-12) has about 5k recent downloads and a small
    team (62 stars).
  - `epserde` and `sucds` are more widely used, at about 128k and 117k recent
    downloads.
  - `fst` is mature.
- **Licence.** webgraph is Apache-2.0 OR MIT; fst is MIT OR Unlicense.

### 4.3 Columnar formats (Parquet, Lance, Vortex)

- **Parquet (arrow-rs 60.2.0, 2026-11-21, Apache-2.0).**
  - ColumnIndex and OffsetIndex let readers skip to the pages that hold a
    point lookup or range on a sorted column
    ([PageIndex.md](https://github.com/apache/parquet-format/blob/master/PageIndex.md)).
  - arrow-rs supports `RowSelection` and `RowFilter` and an async reader with
    a suffix-request metadata fetch
    ([arrow_reader](https://docs.rs/parquet/latest/parquet/arrow/arrow_reader/index.html),
    [async_reader](https://docs.rs/parquet/latest/parquet/arrow/async_reader/index.html)).
  - The Lance paper's random-access test set 8 KiB Parquet pages and turned
    off compression and dictionary encoding
    ([arXiv HTML](https://arxiv.org/html/2504.15247v1)).
  - The graph would be an edges table sorted by `(src, kind)` plus a
    reverse-sorted copy.
- **Lance (Apache-2.0, Rust core).**
  - It reads local disk and S3-compatible stores and offers versioning
    ([lance-format/lance](https://github.com/lance-format/lance)).
  - Its paper targets at most one IOP per value for fixed-width columns and
    two for variable-width (arXiv HTML above).
  - LanceDB declared the 2.1 file format stable
    ([LanceDB blog](https://www.lancedb.com/blog/lance-file-2-1-stable)). The
    stability date is **[unverified]**.
  - The crate is 14.0.3 on crates.io, and `v15.0.0-beta.27` was tagged on
    2026-11-22.
  - It has a non-optional dependency on `datafusion`.
- **Vortex (Apache-2.0, LF AI & Data incubation).** It claims "100x faster
  random access reads (vs. modern Apache Parquet)", "10-20x faster scans" and
  "similar compression ratios". The file format has been
  backward-compatible since 0.36.0, while library APIs may still change
  ([vortex-data/vortex](https://github.com/vortex-data/vortex)). The current
  version is 0.93.0 (2026-11-06).
- **Fit.** These formats suit wide, scan-heavy data. Yggdrasil's workload is
  short point lookups plus capped breadth-first search. The Vortex multipliers
  are the project's own figures, without published methodology. Using one of
  these formats brings in Arrow-shaped data and a sizeable dependency tree.
  They become more attractive if the M3 vector segment uses Lance anyway
  (RFC 0001 §11 Q2).

### 4.4 DuckDB and DuckPGQ

- **DuckDB.** v2.0.1 (2026-11-20) and `duckdb-rs` 1.20001.0, both MIT. The
  Rust crate either links a prebuilt library or compiles the C++ from source
  with `bundled` ([duckdb-rs](https://github.com/duckdb/duckdb-rs)). It reads
  Parquet over HTTP and S3 with range requests. A read-only `ATTACH` of a
  remote `.duckdb` file fetches "only the blocks required by a query"
  ([httpfs](https://duckdb.org/docs/2.0/core_extensions/httpfs/https.html)).
- **DuckPGQ.** It adds SQL/PGQ `GRAPH_TABLE` queries through DuckDB's grammar
  extension API and is a community extension at v0.10.1. It calls itself "a
  research project and a work in progress"
  ([cwida/duckpgq-extension](https://github.com/cwida/duckpgq-extension)).
  Its CIDR 2023 design builds a CSR on the fly during query execution and
  targets many-to-many shortest paths
  ([CIDR 2023 listing](https://vldb.org/cidrdb/2023/duckpgq-efficient-property-graph-queries-in-an-analytical-rdbms.html)).
  The CSR detail comes from the authors' talk slides as summarised in a
  search result, so it is **[unverified]** against the full paper.
- **Fit.** DuckPGQ is an analytical engine. Yggdrasil's Verbs are OLTP-style
  point traversals, which would pay planner and vectorised-execution overhead
  per call. That overhead is **[unmeasured]**. DuckPGQ also exposes a query
  language, which ADR 0003 forbids on the public surface. Internal use would
  be allowed.

### 4.5 Kùzu and its successors

- **Kùzu.** The repository was archived on 2025-10-10, and its README says
  "Kuzu is working on something new!". The last release is 0.11.3, under MIT
  ([kuzudb/kuzu](https://github.com/kuzudb/kuzu)). The University of Waterloo
  reports that Apple "reached an agreement in October 2025 to purchase all
  shares of the company"
  ([UWaterloo CS](https://cs.uwaterloo.ca/news/waterloo-based-graph-database-start-up-kuzu-acquired-apple)).
  ADR 0004 had already rejected Kùzu over this risk.
- **LadybugDB.** It is the active fork, renamed from Kùzu. The core is C++,
  with a Rust binding published as `lbug`. It keeps Kùzu's columnar storage,
  CSR adjacency indexes and Cypher. It released v0.22.3 on 2026-11-07, with
  about 840k recent downloads of `lbug`
  ([LadybugDB/ladybug](https://github.com/LadybugDB/ladybug), crates.io
  `lbug`). Its roadmap lists native SQLite support and Arrow/Parquet
  optimisations, not object storage
  ([Ladybug blog, 2026-03-01](https://blog.ladybugdb.com/post/ladybug-spreading-its-wings/)).
  Remote databases over HTTP need range-request support. The local file cache
  is scoped to one transaction, so "a separate later transaction downloads the
  file again"
  ([Ladybug httpfs](https://docs.ladybugdb.com/extensions/httpfs/)). The
  maintainers are named only as "LadybugDB Developers".
- **Bighorn** (Kineviz) is archived (last push 2026-08-10)
  ([Kineviz/bighorn](https://github.com/Kineviz/bighorn)).
- **RyuGraph** (Predictable Labs) last released v25.9.2 on 2025-12-06 and
  shows no licence that GitHub's licence detection recognises
  ([predictable-labs/ryugraph](https://github.com/predictable-labs/ryugraph)).
- **Fit.**
  - **Speed:** a strong engine for multi-hop analytics.
  - **Format:** a mutable, transactional database, not an immutable artifact.
  - **Build:** links a C++ core into the Rust binary.
  - **Language:** Cypher, which ADR 0003 forbids on the public surface.
  - **Ownership:** a fork about 13 months old, with anonymous maintainers.

### 4.6 Other embedded, Rust-friendly graph stores

- **HelixDB** (Apache-2.0, Rust) is a graph, vector and BM25 database "built
  in Rust on object storage". It has a single writer and auto-scaling readers
  in its Cloud offering, and an openCypher profile. It released v3.7.0 on
  2026-11-11 ([HelixDB/helix-db](https://github.com/HelixDB/helix-db)). It is
  a transactional database, not a per-revision artifact format, and its
  object-storage read path is **[unverified]** beyond the README.
- **CozoDB** (MPL-2.0) has not released since v0.7.6 on 2023-12-11
  ([cozodb/cozo](https://github.com/cozodb/cozo)).
- **IndraDB** (MPL-2.0) released v5.0.0 on 2025-08-16 after a 29-month gap
  ([indradb/indradb](https://github.com/indradb/indradb)).
- **Oxigraph** (Apache-2.0) is active, but it is an RDF/SPARQL store and does
  not match the property-graph Verbs
  ([oxigraph/oxigraph](https://github.com/oxigraph/oxigraph)).
- **Glean**, Meta's code-fact system, is the closest precedent for code facts.
  Its databases are identified as `<name>/<hash>`, uploaded to S3 when
  complete and downloaded by servers. It uses stacked incremental databases
  ([Glean databases](https://glean.software/docs/databases/),
  [incrementality](https://glean.software/docs/implementation/incrementality/)).
  It is Haskell, not an embeddable Rust library.

### 4.7 Object-storage-native designs (patterns, not libraries)

turbopuffer and Quickwit share a recipe that ADR 0005 already follows:

- object storage as the source of truth;
- immutable segments;
- metadata in a relational metastore;
- stateless query nodes that cache on NVMe;
- cold queries bounded by a fixed, small number of roundtrips.

Quickwit's hotcache adds one piece the current Shard design lacks: a small
per-artifact "open cheaply" structure (§3).

## 5. Cross-repo traversal without N-Shard fan-out

Questions like "who calls this API across 5000 repos" ([PRD](../PRD.md)) need
an index from a global symbol to the Shards that reference it. Options:

1. **Cross-repo edge index in Postgres.** ADR 0005 already plans this, and the
   v1 schema has `DEPENDS_ON` and `PROVIDES` edges (RFC 0001 §5).
   - **How:** each publish replaces one repo's rows of
     `(global_symbol, repo, local_nid, kind)`.
   - **Cost:** write amplification on every publish, and a table whose size
     depends on the cross-repo edge ratio, which is **[unmeasured]**.
   - **Benefit:** the query-time cost is one indexed lookup.
2. **Package-level routing.** Sourcegraph keeps per-upload rows of the
   packages each index provides (`lsif_packages`) and references
   (`lsif_references`), keyed on `(scheme, name, version)`. Per-upload symbol
   names live in a prefix-tree table, `codeintel_scip_symbol_names`
   ([schema.md](https://raw.githubusercontent.com/sourcegraph/sourcegraph-public-snapshot/main/internal/database/schema.md),
   [schema.codeintel.md](https://raw.githubusercontent.com/sourcegraph/sourcegraph-public-snapshot/main/internal/database/schema.codeintel.md);
   archived snapshot, last pushed 2024-09).
   - **Benefit:** this is cheap to maintain and bounds fan-out to the
     dependents of a package.
   - **Cost:** a popular internal library can still have thousands of
     dependents. How the dependent count is distributed is
     **[unmeasured]**.
3. **A global immutable symbol index segment.** This is an FST or sorted
   string table on object storage mapping a global symbol to postings of
   `(repo, nid)`. It would be built per publish as small segments and
   periodically merged, in the way tantivy and Quickwit manage segments and
   splits.
   - **Benefit:** it keeps billions of rows out of Postgres (ADR 0005 rejected
     "everything-in-Postgres").
   - **Cost:** merge and garbage-collection machinery, plus freshness lag
     between publish and merge.

All three need the stable, version-free Symbol id that RFC 0001 §5 defers to
M1. SCIP's grammar `<scheme> <manager> <package-name> <version>
<descriptor>+` ([scip.proto](https://github.com/scip-code/scip/blob/main/scip.proto))
gives the global key once the version is dropped.

## 6. Comparison

| | Warm traversal | Shard size (M0 graph) | Cold fetch / partial load | Cross-repo help | Rust maturity | Licence |
|---|---|---|---|---|---|---|
| SQLite, today's schema | µs, measured | ≈ 460–530 B/edge | whole file today; page VFS possible | none | high (rusqlite) | public domain / MIT |
| SQLite, interned + `WITHOUT ROWID` | ≈ 2× faster, measured | ≈ 54–57 B/edge | same; 56 KiB interior pages make a hotcache cheap | none | high | public domain / MIT |
| Custom CSR (+ WebGraph, + fst) | array reads; Verb code to write | ≈ 22–24 B/edge raw | section offsets give ~1–3 range reads per hop (estimate) | an fst could serve as the global index | medium (fst high; webgraph small) | Apache/MIT |
| Parquet (arrow-rs) | page-index point lookups | unmeasured | mature range reads and page skipping | none | high | Apache-2.0 |
| Lance | claims 1–2 IOPS per value | unmeasured | native object store | none | medium-high; heavy deps | Apache-2.0 |
| Vortex | claims 100× Parquet | unmeasured | layouts over object store | none | medium (pre-1.0 API) | Apache-2.0 |
| DuckDB + DuckPGQ | analytical engine; per-query CSR | unmeasured | range-read `ATTACH` | none | DuckDB high; DuckPGQ research | MIT |
| LadybugDB (Kùzu fork) | fast multi-hop (vendor claim) | unmeasured | per-transaction whole-file cache | none | binding over C++; young fork | MIT |
| HelixDB | unmeasured | unmeasured | object-storage-backed (README) | none | active, Rust | Apache-2.0 |

## 7. Options for Yggdrasil, with costs

Not ranked, and nothing is chosen. Every Shard-layout change bumps
`SCHEMA_VERSION`, which means a fleet-wide re-index and no lazy migration
(RFC 0001 §11 "Closed: Shard schema evolution").

- **A. Keep SQLite, fix the schema.**
  - **Changes:**
    - integer `nid`, with string ids stored once;
    - interned paths and integer kinds and provenance;
    - packed sites;
    - clustered `WITHOUT ROWID` edges with one covering reverse index;
    - `VACUUM`;
    - `immutable=1` and `mmap_size` on read.
  - **Benefit:** ≈ 9× smaller, ≈ 2× faster warm **[measured]**. The tooling
    stays the same (sqlite3 CLI, `dbstat`).
  - **Cost:**
    - one schema bump;
    - a rewrite of the Verb SQL, which must translate external ids to `nid`
      at the edge of each query;
    - the edge-uniqueness checks move into the writer or onto the primary
      key.

  This is the lowest-risk option.
- **B. A plus partial loading.**
  - **Changes:**
    - range-read the graph segment through a read-only VFS built on
      `object_store`;
    - ship a hotcache in the manifest: the interior pages plus the
      schema page;
    - replace whole-file SHA-256 with per-page or per-block checksums.
  - **Benefit:** a cold first Verb would not wait for the whole segment.
  - **Cost:**
    - writing and fuzzing a VFS, either over a third-party crate or unsafe
      FFI. Graft, the most capable Rust option, is alpha;
    - a new integrity model;
    - new failure modes: per-page timeouts, and signals if mmap and remote
      I/O are mixed.

  It only pays off if whole-segment GET time is a material part of cold
  latency, which depends on measurement 3 in §8.
- **C. Custom CSR graph segment.**
  - **Changes:**
    - forward and reverse CSR with per-edge attribute columns;
    - an `fst` string table;
    - an optional WebGraph encoding later;
    - a footer of section offsets for range reads;
    - SQLite kept, or not, for history and summaries.
  - **Benefit:** ≈ 20× smaller than today, ≈ 2.4× smaller than A
    **[measured, raw]**, with a bounded roundtrip count when cold.
  - **Cost:**
    - a bespoke, versioned binary format;
    - hand-written query code for each of the 12 planned graph Verbs;
    - its own debugging tools;
    - filters (kind, `since`, provenance) need their own indexes or scans.
- **D. Columnar segment (Lance, Vortex or Parquet).**
  - **Benefit:** mature range reads, compression and a possible shared format
    with the M3 vectors.
  - **Cost:**
    - an Arrow and DataFusion-sized dependency tree, and build time;
    - point-lookup performance that is only claimed for this workload;
    - Vortex's library API is still moving.
- **E. Embedded graph engine (LadybugDB).**
  - **Benefit:** Cypher and analytics internally, and fast multi-hop.
  - **Cost:**
    - a C++ build in a single Rust binary;
    - a mutable-database format rather than an immutable artifact;
    - its remote reads re-download the file per transaction;
    - fork governance risk, the same risk ADR 0004 rejected Kùzu for.
- **F. DuckDB (± DuckPGQ) as the segment engine.**
  - **Benefit:** a mature engine with range-read `ATTACH`.
  - **Cost:**
    - a C++ build;
    - analytical-engine overhead per point Verb, which is unmeasured;
    - DuckPGQ is research-grade and an extension loaded at runtime.

For cross-repo work, X1 (Postgres edge index), X2 (package routing) and X3
(global symbol segment) from §5 combine with any of A–F. X2 plus X1 or X3 is
the common shape: route by package, then answer from an index.

## 8. What must be measured before deciding

1. **Precise-pass growth.** Bytes per LOC once M1 adds SCIP REFERENCES and
   occurrences, for Go, TypeScript, Python and Rust, under layouts A and C.
2. **Org shape.** The distribution of repo LOC (p50, p99, monorepos), to get
   per-Shard sizes and the total working set for NVMe sizing.
3. **Cold fetch against a real store.** Compare a whole-segment GET for 5–50 MB
   with a range-read VFS (with and without a hotcache), on S3, S3 Express and
   a self-hosted S3-compatible store. Measure time-to-first-Verb, not
   throughput.
4. **Warm Verbs in Rust.** Run `neighbors` at depth 3 with caps, on hub nodes,
   under A and C. Python numbers are only an upper bound.
5. **Cross-repo cardinalities.** The fraction of edges that cross repos, the
   publish write amplification for X1, and the dependents-per-package
   distribution for X2.
6. **Dependency cost.** Build time and binary size for `lance`,
   `duckdb[bundled]` and `lbug` against today's `yg`.
7. **Re-index cost.** One full fleet rebuild, since every layout option costs
   a schema bump (RFC 0001 §11).

## 9. Unverified or uncertain

- The 100M-LOC extrapolations (§2.3) rest on two Rust crates and the
  syntactic pass only.
- Code-graph compression with WebGraph (§4.2).
- The stability date of Lance 2.1 (§4.3).
- Lance's and Vortex's random-access multipliers are vendor claims (§4.3).
- DuckPGQ's on-the-fly CSR detail comes from talk slides via a search summary
  (§4.4).
- HelixDB's object-storage read path (§4.6).
- The roundtrip estimates for B and C (§3, §4.2) are design arithmetic, not
  measurements.
- Release dates and download counts come from the GitHub and crates.io APIs
  on 2026-11-23 and will drift.
- A larger, registry-scale Shard build was abandoned when the host ran low on
  disk. No large-Shard data point exists.

## Appendix A: measurement method

All measurements were taken on 2026-11-23 on Apple silicon (macOS), with
SQLite 3.54.0.

1. **Build the Shards.** A scratch binary called
   `yg_index::syntactic_pass(root)` and then
   `yg_shard::write_shard(LocalFileSystem, …)` at baseline `c4bec0c`, on the
   cargo-registry sources of `tokio-1.54.1` and `tantivy-0.26.1`.
2. **Count LOC** as `wc -l` over `*.rs`.
3. **Break down space** with `dbstat` over the produced `graph.sqlite`.
4. **Build the variants.** A Python script read the same file and wrote the
   interned SQLite variant (with `INSERT OR IGNORE` deduplicating on
   `(src, kind, dst, site)`, which kept all 91 252 tokio edges) and the raw
   CSR file.
5. **Compress** with `zstd -3`.
6. **Time lookups** with Python `sqlite3` on both files, as described in §2.3.
