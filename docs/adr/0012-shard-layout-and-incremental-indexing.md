# Tuned SQLite Shards with loose segments, complete revisions and a symbol-level cross-repo index

Status: accepted. Refines [ADR 0005](0005-object-storage-native-shards.md): Shards stay immutable per-repo artifacts on object storage behind a Postgres control plane, and this ADR fixes what a Shard holds, how it is fetched, how a revision is built cheaply, and where cross-repo references live. Supplies the storage that [ADR 0011](0011-aggregated-budgeted-decisive-payloads.md) asks for.

The [Shard profile](https://github.com/randax/yggdrasil/blob/563e9b4c3b6a42e9583452055150a2154173b0c3/docs/research/shard-profile.md) found the problem is size, not traversal speed. Warm one-hop lookups take about 10 µs, but the median Shard costs 642 bytes per edge, 91–97% of `graph.sqlite` is the edges table and its four indexes, and a cold call downloads the whole graph segment at about 6.2 ms per MB, so webpack's 1.05 GB Shard took 6 s on first touch. Indexing wall time is mostly the full-history clone (810 s against 258 s of indexing for the corpus). The [storage-engine research](https://github.com/randax/yggdrasil/blob/bc0b8298369eed9d87eae3c2aa0718ea0826a34f/docs/research/graph-storage-engines.md) and the [incremental-indexing research](https://github.com/randax/yggdrasil/blob/7b32354068546788af58061a218478c7663ffb51/docs/research/incremental-indexing.md) set out the options; this ADR picks among them.

## The decisions

### The graph segment is tuned SQLite

The graph segment stays SQLite, rebuilt for size:

- node ids are integers inside the segment, and paths, names and other repeated strings are interned in one string table, so no edge row repeats a string id;
- edges live in clustered `WITHOUT ROWID` tables keyed for the two traversal orders: outbound by `(src, site, …)`, so all of one source's sites and their candidates are contiguous, and inbound by `(dst, kind, src, site)`, so each `(src, dst, kind)` group's evidence is contiguous. These are the orders ADR 0011's discovery batches resume on;
- the segment is written once with a fixed page size, vacuumed, and opened `immutable=1` with mmap.

The research measured this layout at 8.6–9.3× smaller (about 55 bytes per edge) and about 2× faster warm lookups than the M0 schema. Public node ids are unchanged: the integer ids never leave the segment.

### Every candidate at an ambiguous call site is stored, once per site

94% of syntactic CALLS edges sit at call sites with several same-named candidates. The Shard stores every candidate, because ADR 0011 never hides an inbound caller and `impact` must not silently lose a caller that ranked seventh. A call site is stored once, with its candidate list; confidence is derived at read time from [ADR 0006](0006-syntactic-ambiguity-spread-confidence.md)'s rule and locality, not stored per edge. Inside a Precise Root that has precise coverage, precise edges replace the syntactic candidates.

### One copy of source text, in a contents segment

Each Shard has a contents segment: every indexed file, zstd-compressed, with a line-offset table. It is the only copy of source text in a Shard. ADR 0011's declaration lines and call-site lines, and Symbol bodies, are sliced from it by file and span. Tantivy indexes bodies but no longer stores them.

A trigram segment (Zoekt-style) has a reserved place in the Shard layout. It is built only when regex search ships after v1, and then regex and diff-content search become a `search` mode without another layout decision.

### The vectors segment is separate

When semantic search lands, each Shard gains a `vectors` segment: one vector per Symbol, built from its body in the contents segment, quantized to int8, with a memory-mapped ANN graph. The embedding model and its version are recorded in the manifest, so changing the model rebuilds only this segment. The graph segment never holds vectors. The ANN library and the embedding model stay open (RFC 0001 §11), but they fit this segment.

### A Shard is a manifest plus loose segment files

A Shard revision is `manifest.json` plus each segment's files as separate objects: the graph segment, tantivy's files (merged to one segment at build), the contents segment and, later, the vectors and trigram segments. The manifest lists each file with its size and sha256, plus the input hash of each Precise Root. There is no archive: the cache downloads the files a Verb needs into the revision's directory, verifies each, and keeps exactly one copy. Verbs that need no vectors never fetch them. GC still deletes a revision prefix, because every revision is complete.

### Cold fetch downloads whole files, off the request path where possible

A cold miss downloads each needed file whole, as parallel byte-range GETs (the `object_store` crate already coalesces and parallelises ranges), and checks its sha256. When a new revision is published, the serving nodes that hold the repo download it before they switch to it, so a cold fetch on the request path happens only after a restart, an eviction or a repo's first query. With the tuned layout, the measured worst case drops from about 6 s to under 1 s.

Ranged page reads stay possible later: the fixed page size and contiguous interior pages mean a remote VFS with a hotcache of interior pages (Quickwit-style) can be added without a format change if precise-pass segments outgrow whole downloads. It is not built now.

### Every revision is a complete Shard; indexing gets cheaper upstream of it

No delta or overlay Shards. Each published revision is complete, so every Verb reads one revision and the clustered contiguity, discovery-batch resume and view-computed counts above hold without merging layers. Indexing gets cheaper before publication instead:

- **Per-stage worker metrics.** Clone and fetch, archive, history, parse, resolve, segment build and upload are timed for every job. They decide when the follow-up below is worth building.
- **A persistent mirror per worker.** Each worker keeps a bare, blobless partial clone of each repo it has indexed and fetches incrementally. History extraction reads only commits and trees (`git log --name-only`), so it needs no blobs; the syntactic pass fetches the current tree's blobs on demand.
- **Incremental history.** A job walks `git log <previous>..<new>` and merges the result with the previous revision's history rows. A force-push or any rewrite that removes the previous commit from the branch's ancestry falls back to a full walk.
- **A per-blob extraction cache.** Per-file facts and fingerprints are cached by git blob id plus extractor, grammar and schema versions, so an unchanged file is not parsed again. A miss only costs a parse, so the cache can live on the worker.

Resolution, segment build and upload stay proportional to the repo, which the tuned layout keeps small. Incremental resolution by name (persist each file's declaration hash and re-resolve only call sites whose candidate set changed, since ADR 0006's confidence depends on the repo-wide candidate count) is a named follow-up, built when the metrics show resolve taking a large share of the syntactic freshness target on real repos, and gated by a property test that an incremental build equals a full one.

### The precise pass builds per Precise Root and carries unchanged roots forward

A repo has one Shard. Inside it, each build manifest (a Cargo workspace, a `package.json` project, a `go.mod` module, and so on) defines a Precise Root. A root's input hash covers its files, its lockfile, the indexer and its version, and the input hashes of the in-repo roots it depends on. A precise build runs only for roots whose input hash changed.

Each new revision carries forward the precise edges of every root whose input hash is unchanged from the revision they came from. A root whose hash changed shows its syntactic edges until its precise build lands. A precise result is applied only to a revision whose root input hash equals the one the build ran on; if the root has changed again meanwhile, the result is discarded and the root is built at the new hash. Because a root's hash includes its in-repo dependencies, an edited library forces its dependents to rebuild, so no carried edge points into changed code. Every edge already carries its Provenance, so an answer always shows which parts are precise.

Build caching (shared registry caches, sccache or Nix) stays an open question for the precise-pass work (RFC 0001 §11).

### Cross-repo references are a symbol-level index in Postgres

Each published revision writes one row per external symbol its repo references: the version-free moniker, the referencing repo, and the reference count. Rows come from precise monikers and from syntactic import resolution, with the latter's confidence kept. A repo's rows are replaced in the same Postgres transaction as its `shards` pointer swap. `callers(X)` and `impact` across repos filter these rows to the caller's effective view and then open only the Shards of repos that reference X; view-scoped cross-repo degrees are a sum over the visible rows. Package routing (`repo_packages`, `xrepo_edges`) stays, and serves `deps` and `dependents`.

A repo's own counts can be computed inside its Shard, because Visibility is per repo: a repo is in the view or not. Any count that crosses repos comes from the Postgres rows above, filtered per view, never from a total stored in a Shard.

## Considered options

- **A custom CSR file for the graph segment.** About 23 bytes per edge, 2.4× smaller again and memory-mappable, but a custom format, writer and reader, and per-Verb query code with no SQL to fall back on. Revisit only if tuned SQLite proves too large; the Verb layer hides the change.
- **Another engine** (LadybugDB, DuckDB with DuckPGQ, Lance or Vortex). Kùzu is archived and its forks are young, DuckPGQ is research-grade, and Lance pulls in DataFusion. None beats tuned SQLite on small segments and point lookups.
- **Page-level remote reads now, or only above a size threshold.** Cold first calls become a few roundtrips instead of a download, but traversals over many leaf pages chain roundtrips, integrity needs a hash per block, and the VFS needs unsafe FFI we would own. Kept as the escape hatch above.
- **Line text inline in the graph segment, tantivy keeps bodies.** Text in two places, and regex later needs a format change.
- **Building the trigram index now.** It would cost 2–3× the source size before regex exists.
- **Delta or overlay Shards (Zoekt-style), with or without ownership sets (Glean-style).** Publication proportional to the change, but every Verb merges layers and filters tombstones, which breaks clustered contiguity, exact batch resume and per-view counts, and GC must pin bases.
- **Serving the nearest precise revision with diff-translated positions (Sourcegraph-style).** Fewer precise builds, but read-path diffing and answers that mix revisions, with fingerprints checked against one revision and precise edges from another.
- **A whole-repo precise build on every change.** Simplest, but large monorepos lose precision on every push for the length of the build.
- **Package-level cross-repo routing only.** A `callers` on a popular internal library opens every Shard that depends on its package, even when few call the Symbol.
- **A global immutable symbol segment on object storage.** Fast lookups, but rebuilt or layered on every publish, which brings back the staleness and compaction rejected for overlays.
- **Capping stored candidates per site, or dropping candidates above a candidate-count threshold.** Two to three times smaller, but inbound callers vanish, concentrated on the commonest names, and `impact` misses real callers where no precise pass runs.
- **sqlite-vec inside the graph segment.** Every cold graph fetch would also download the vectors, and a model change would rewrite the graph file.
- **A global ANN index across repos.** It breaks per-repo scoring before the merge, and every publish updates a shared index.

## Consequences

- The M1 schema bump ([RFC 0001](../rfc/0001-yggdrasil-v1.md) §11, "Shard schema evolution") carries the tuned graph segment, the contents segment and loose-file packaging, so the fleet re-indexes once for all three, together with stable Symbol IDs.
- A worker's mirror and blob cache make job placement matter: a job on a worker without the mirror pays a fresh blobless clone. Routing a repo's jobs to the worker that holds its mirror is an optimisation, never a correctness requirement.
- A repo whose Precise Root changes faster than its build completes stays syntactic for that root. That is visible per root in admin status and in edge Provenance, and is accepted.
- Postgres write volume per publish grows with the repo's external symbol references; it is bounded by that repo and replaced, not appended.
