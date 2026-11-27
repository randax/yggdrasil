# Tuned SQLite Shards with loose segments, complete revisions and a symbol-level cross-repo index

Status: accepted. Refines [ADR 0005](0005-object-storage-native-shards.md): Shards stay immutable per-repo artifacts on object storage behind a Postgres control plane, and this ADR fixes what a Shard holds, how it is published and fetched, how a revision is built cheaply, and where cross-repo references live. Supplies the storage that [ADR 0011](0011-aggregated-budgeted-decisive-payloads.md) asks for.

The [Shard profile](https://github.com/randax/yggdrasil/blob/563e9b4c3b6a42e9583452055150a2154173b0c3/docs/research/shard-profile.md) found the problem is size, not traversal speed. Warm one-hop lookups take about 10 µs, but the median Shard costs 642 bytes per edge, 91–97% of `graph.sqlite` is the edges table and its four indexes, and a cold call downloads the whole graph segment at about 6.2 ms per MB on a local store, so webpack's 1.05 GB Shard took 6 s on first touch. Indexing wall time is mostly the full-history clone (810 s against 258 s of indexing for the corpus). The [storage-engine research](https://github.com/randax/yggdrasil/blob/bc0b8298369eed9d87eae3c2aa0718ea0826a34f/docs/research/graph-storage-engines.md) and the [incremental-indexing research](https://github.com/randax/yggdrasil/blob/7b32354068546788af58061a218478c7663ffb51/docs/research/incremental-indexing.md) set out the options; this ADR picks among them.

## The decisions

### The graph segment is tuned SQLite

The graph segment stays SQLite, rebuilt for size:

- node ids are integers inside the segment, and paths, names and other repeated strings are interned in one string table, so no edge row repeats a string id;
- edges live in clustered `WITHOUT ROWID` tables keyed for the two traversal orders: outbound by `(src, site, …)`, so all of one source's sites and their candidates are contiguous, and inbound by `(dst, kind, src, site)`, so each `(src, dst, kind)` group's evidence is contiguous. These are the orders ADR 0011's discovery batches resume on;
- the segment is written once with a fixed page size, vacuumed, and opened `immutable=1` with mmap.

The research measured a close variant of this layout (one clustered edge table plus one reverse index, on two Rust crates, syntactic only) at 8.6–9.3× smaller, about 55 bytes per edge, and about 2× faster warm lookups than the M0 schema. The layout here differs (both orders carry the site, and precise evidence is added), so that figure is a planning estimate, re-measured by the M1 schema bump's eval run. Public node ids are unchanged: the integer ids never leave the segment.

### Every candidate at an ambiguous call site is stored

94% of syntactic CALLS edges sit at call sites with several same-named candidates. The Shard stores every candidate, because ADR 0011 never hides an inbound caller and `impact` must not silently lose a caller that ranked seventh. In the outbound order a call site is one row holding its candidate list; the inbound order necessarily has one entry per candidate. Confidence is not stored per candidate: it is [ADR 0006](0006-syntactic-ambiguity-spread-confidence.md)'s 0.9/N, derived at read time. Locality only ranks candidates (ADR 0011); it never changes confidence.

Precise evidence replaces syntactic evidence per call site, never per root. The precise result lists the files it indexed. In those files, a site with a precise occurrence keeps only its precise evidence; a site the precise output did not resolve keeps its syntactic candidates; and files the precise build never indexed (feature-gated code, files outside the build's configuration, generated code) keep all their syntactic evidence. So a site "seen by both passes" in ADR 0011 collapses to its precise observation in the Shard, and nothing SCIP did not examine loses a caller.

### Edges to other repos are resolved per view at read time

A Shard never resolves an edge into another repo. A precise reference to an external Symbol is stored as an edge to its version-free moniker. At read time the moniker is resolved to the Symbol in a providing repo that is in the caller's effective view; if no provider is in the view, because it is hidden or was never admitted, the edge is dropped, the same way in both cases ([ADR 0008](0008-team-granted-repository-visibility.md)). Counts computed inside a Shard (degrees, `sites: n`, `+N`, hub decisions, work caps) cover only edges inside the repo, which are visible exactly when the repo is. External edges are counted only once resolved into the view.

### One copy of source text, in a contents segment

Each Shard has a contents segment: every file the indexer parsed (files of a language it extracts, plus markdown), zstd-compressed, with a line-offset table. Binary files, files over the indexer's size cap and files the repo's index rules exclude are not stored. It is the only copy of source text in a Shard. ADR 0011's declaration lines and call-site lines, Symbol bodies and `search` snippets are sliced from it by file and span, and a Verb returns only spans tied to the nodes, edges or hits it returns. Tantivy indexes bodies but no longer stores them.

A trigram segment (Zoekt-style) has a reserved place in the layout. It is built only when regex search ships after v1, and then regex and diff-content search become a `search` mode without another layout decision.

### The vectors segment is separate

When semantic search lands, each Shard gains a `vectors` segment: one vector per Symbol and per Doc Section, built from text in the contents segment, quantized to int8, with a memory-mapped ANN graph. The embedding model and its version are recorded in the manifest, so changing the model rebuilds only this segment. The graph segment never holds vectors. The ANN library and the embedding model stay open (RFC 0001 §11).

### A Shard is a manifest plus loose segment files

A Shard revision is `manifest.json` plus each segment's files as separate objects: the graph segment, tantivy's files (merged to one segment at build), the contents segment and, later, the vectors and trigram segments. The manifest lists each file with its size and sha256, the state of each Precise Root (precise, with the artifact it used, or syntactic), and the embedding model. There is no archive.

A query node's cache stores files by sha256 and links them into a revision's directory, so a file unchanged between revisions (the contents segment when only a precise result landed) is downloaded once. Each Verb fetches only the files it needs: `node`, `neighbors` and the other graph Verbs need the graph and contents segments, `search` needs the full-text and contents segments, and semantic search needs the vectors segment.

### Publishing a revision: serialised, pre-warmed, then one atomic swap

- **Revision ids.** A revision id is derived from the commit, the schema version, and a digest of the precise artifacts the revision used. One commit can therefore have several revisions (syntactic first, then one per landed precise result), each immutable and each with its own object prefix. The same inputs always give the same id, so republishing is idempotent, and RFC 0001 §11's schema-outdated check still finds revisions by their schema suffix.
- **Serialised per repo.** The pointer swap is a compare-and-swap on the revision the publisher composed from. A publisher whose base is no longer current composes again from the current revision and retries, so a late job can never roll back a newer one.
- **Pre-warm before the swap.** The publisher uploads every file, records the revision as pending, and waits a bounded time (about a minute) while the query nodes that hold the repo download the changed files. Then it swaps regardless.
- **One atomic swap.** The pointer, the repo's cross-repo rows (below) and RFC 0001 §5.1's Memory staleness checks commit in one Postgres transaction. Every request reads its revision pointers and cross-repo rows from one Postgres snapshot and opens exactly those revisions, fetching cold if a node is not warm. No node serves an older revision because it has not switched yet.
- **Cold path.** A cold fetch downloads each needed file whole, as parallel byte-range GETs (the `object_store` crate coalesces and parallelises ranges), and checks its sha256. With the tuned layout the worst repo in the profile would drop from about 6 s to under 1 s on the same local store. That is derived, not measured, and real object storage is slower.
- **Escape hatch.** The fixed page size and contiguous interior pages keep a remote VFS with a hotcache of interior pages (Quickwit-style) possible without a format change. It is built if the M3 certification run shows a cold fetch for any repo exceeding the PRD's cold-path target.
- **Retention.** A superseded revision's objects are kept for at least the cursor lifetime (an hour, ADR 0011) plus the longest request time, so a valid cursor can always fetch the revisions it pinned. GC then deletes the revision prefix.

### Every revision is a complete Shard; indexing gets cheaper upstream of it

No delta or overlay Shards. Each published revision is complete, so every Verb reads one revision and the clustered contiguity, discovery-batch resume and view-computed counts above hold without merging layers. Indexing gets cheaper before publication instead:

- **Per-stage worker metrics.** Clone and fetch, archive, history, parse, resolve, segment build, upload and publish are timed for every job. They decide when the follow-up below is worth building.
- **A persistent mirror per worker.** Each worker keeps a bare, blobless partial clone of each repo it has indexed and fetches incrementally. History extraction reads only commits and trees (`git log --name-only`), so it needs no blobs; the syntactic pass fetches the current tree's blobs on demand. Routing a repo's jobs to the worker holding its mirror is an optimisation, never a correctness requirement.
- **Incremental history.** A job walks `git log <previous>..<new>` and merges the result with the previous revision's history rows. A force-push or any rewrite that removes the previous commit from the branch's ancestry falls back to a full walk.
- **A per-blob extraction cache.** Per-file facts are cached by git blob id, language, and extractor, grammar and schema versions. Cached facts are path-independent: path-derived ids are materialised after lookup, so the same blob at a new path is reused correctly. A miss only costs a parse, so the cache can live on the worker.

Resolution, segment build and upload stay proportional to the repo, which the tuned layout keeps small. Incremental resolution by name (persist each file's declaration hash and re-resolve only call sites whose candidate set changed, since ADR 0006's confidence depends on the repo-wide candidate count) is a named follow-up, built when the metrics show resolve taking a large share of the syntactic freshness target on real repos, and gated by a property test that an incremental build equals a full one.

### The precise pass builds per Precise Root and reuses unchanged results

A repo has one Shard. Inside it, each build manifest (a Cargo workspace, a `package.json` project, a `go.mod` module, and so on) defines a Precise Root. A file belongs to the root of its nearest enclosing manifest, so a nested manifest owns its subtree; files under no manifest belong to no root and stay syntactic.

A root's input hash covers:

- its own files, manifest and lockfile;
- every workspace-level file its ecosystem's build reads, such as a shared lockfile, `go.work`, workspace manifests, `.cargo/config.toml` or `tsconfig` bases (the precise-pass work names these per ecosystem; where an ecosystem's input closure is not known, the hash covers the whole repo tree);
- the sandbox image digest, the indexer and its version, and the version of the step that turns SCIP into graph evidence;
- the input hashes of the in-repo roots its manifest declares as dependencies.

A precise result is stored as a per-root artifact in object storage, keyed by repo, root, input hash and the schema version. It also records the hash of every file outside the root that its evidence points into. A revision uses a root's artifact when the artifact matches the root's current input hash, the schema version, and the recorded hashes of those outside files. Otherwise the root shows its syntactic evidence.

A root is queued for a precise build whenever the current revision has no matching artifact for it and no build is in flight: after a change, after a schema bump, or after a discarded result. A failed build is retried with backoff and then left syntactic until the root's hash changes, visible per root in admin status. When an artifact lands, a compose job builds a new revision of the repo's current commit, which picks up every matching artifact, and publishes it through the compare-and-swap above.

A dependency root's change changes its dependents' hashes, so a frequently changed core library leaves its dependents syntactic too until their builds land. That transitive cost is accepted, visible per root, and is the reason no stale precise edge is ever served. Every edge carries its Provenance, so an answer always shows which parts are precise. `shards.provenance_level` records the weakest level across the repo's roots, and the manifest holds the per-root state.

Build caching (shared registry caches, sccache or Nix) stays an open question for the precise-pass work (RFC 0001 §11).

### Cross-repo references are a symbol-level index in Postgres

Each published revision writes, for each external Symbol its repo references precisely, one row per kind: the version-free moniker, the referencing repo, the kind, and the number of distinct referencing Symbols. That is the number of ADR 0011 aggregated `(src, dst, kind)` edges, the unit ADR 0011 counts degrees in. Rows are written only for monikers whose package some admitted repo provides; when a providing repo is admitted later, a background job derives its rows from the current Shards of the repos that depend on that package (`repo_packages`), without re-indexing. A repo's rows are applied as a diff in the same Postgres transaction as its pointer swap.

`callers(X)` and `impact` across repos filter the rows for X's moniker to referencing repos in the caller's effective view, then open only those repos' Shards. X's provider is the repo the caller asked about. When more than one repo in the view provides the moniker's package (a fork or a vendored copy), each is a candidate for the reference, spread like an ADR 0006 candidate set with the denominator computed on the view, so a hidden provider changes nothing. The visible cross-repo degree is the sum of the visible rows, and only precise evidence is counted, so the request's Provenance and confidence filters apply exactly. Package routing (`repo_packages`, `xrepo_edges`) stays and serves `deps` and `dependents`.

A cross-repo cursor stores, with its first page, the view-filtered set of referencing repos and the revision of each, and continuations read exactly those revisions (rechecking the view as ADR 0011 requires), so a republish between pages neither skips nor repeats a result.

The syntactic pass resolves no names outside the repo (ADR 0006), so it contributes no rows. A referencing root without precise coverage (a broken build, or a root waiting for its build after a push) has no cross-repo callers until its precise artifact lands. That is ADR 0002's standing trade, stated here so the routing is not mistaken for a guarantee.

## Considered options

- **A custom CSR file for the graph segment.** About 23 bytes per edge, 2.4× smaller again and memory-mappable, but a custom format, writer and reader, and per-Verb query code with no SQL to fall back on. Revisit only if tuned SQLite proves too large; the Verb layer hides the change.
- **Another engine** (LadybugDB, DuckDB with DuckPGQ, Lance or Vortex). Kùzu is archived and its forks are young, DuckPGQ is research-grade, and Lance pulls in DataFusion. None beats tuned SQLite on small segments and point lookups.
- **Page-level remote reads now, or only above a size threshold.** Cold first calls become a few roundtrips instead of a download, but traversals over many leaf pages chain roundtrips, integrity needs a hash per block, and the VFS needs unsafe FFI we would own. Kept as the escape hatch above.
- **Line text inline in the graph segment, tantivy keeps bodies.** Text in two places, and regex later needs a format change.
- **Building the trigram index now.** It would cost 2–3× the source size before regex exists.
- **Delta or overlay Shards (Zoekt-style), with or without ownership sets (Glean-style).** Publication proportional to the change, but every Verb merges layers and filters tombstones, which breaks clustered contiguity, exact batch resume and per-view counts, and GC must pin bases.
- **Serving the nearest precise revision with diff-translated positions (Sourcegraph-style).** Fewer precise builds, but read-path diffing and answers that mix revisions, with fingerprints checked against one revision and precise edges from another.
- **A whole-repo precise build on every change.** Simplest, but large monorepos lose precision on every push for the length of the build.
- **Replacing a root's syntactic evidence wholesale when its build succeeds.** Simpler, but callers in files or sites the build never examined would vanish.
- **Package-level cross-repo routing only.** A `callers` on a popular internal library opens every Shard that depends on its package, even when few call the Symbol.
- **A global immutable symbol segment on object storage.** Fast lookups, but rebuilt or layered on every publish, which brings back the staleness and compaction rejected for overlays.
- **Capping stored candidates per site, or dropping candidates above a candidate-count threshold.** Two to three times smaller, but inbound callers vanish, concentrated on the commonest names, and `impact` misses real callers where no precise pass runs.
- **sqlite-vec inside the graph segment.** Every cold graph fetch would also download the vectors, and a model change would rewrite the graph file.
- **A global ANN index across repos.** It breaks per-repo scoring before the merge, and every publish updates a shared index.
- **Each node switching to a new revision when its own pre-warm finishes.** Nodes and Postgres would disagree about the current revision, pairing new cross-repo rows and Memory checks with old Shards.

## Consequences

- The M1 schema bump ([RFC 0001](../rfc/0001-yggdrasil-v1.md) §11, "Shard schema evolution") carries the tuned graph segment, the contents segment, loose-file packaging and per-root precise artifacts, so the fleet re-indexes once for all of them, together with stable Symbol IDs.
- A precise result landing publishes a new revision of the same commit. Cursors bind revisions, not commits, so they are unaffected.
- A repo whose Precise Root (or a root it depends on) changes faster than its build completes stays syntactic for that root. That is visible per root in admin status and in edge Provenance, and is accepted.
- Postgres write volume per publish is the diff of that repo's precise external references to admitted packages.
