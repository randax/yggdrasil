# Incremental indexing techniques for code graphs

Status: research note · Date: 2026-11-23 · Question: [#126](https://github.com/randax/yggdrasil/issues/126)
(feeds [#137](https://github.com/randax/yggdrasil/issues/137)) · Yggdrasil baseline: commit `c4bec0c` on `main`

> Scope. How code-intelligence systems re-index a changed revision without
> rebuilding everything, what storage layouts make that possible, how they
> invalidate cross-file and cross-repo edges, how they interact with
> compiler-precise (SCIP) passes, and what this means for Yggdrasil's stable
> Symbol IDs and per-node fingerprints (ADR 0007). The note sets out options
> with their costs and does not choose between them. External claims cite a primary source inline, with
> repository sources pinned to a commit (listed in §8). Claims about Yggdrasil
> cite repository paths at the baseline commit. Anything I could not verify is
> marked **unverified** and collected in §7.

## 1. Summary

- **Every surveyed system that indexes incrementally makes the file (or
  compilation unit) the unit of reuse.** It then picks one of four places to
  absorb the change:
  1. A **per-file content-addressed cache** in front of a full rebuild
     (Graphify, stack-graphs, Kythe's kzip, Bazel's action cache).
  2. A **stacked or overlay index** that hides superseded units and adds new
     ones (Glean's incremental DBs, Zoekt's delta shards, tantivy's segments
     with alive-bitsets).
  3. **Query-time stitching** of per-file partial results, which leaves no
     cross-file state to invalidate (stack graphs; Kythe's union-of-entries
     graph store).
  4. **Not re-indexing at all.** The system serves the nearest indexed commit
     and translates positions through the git diff (Sourcegraph's precise code
     navigation).
- **Cross-file edges are the hard part.** Each system handles them differently:
  - Glean propagates *ownership sets* through the fact graph, so hiding a unit
    never leaves a dangling reference. A derived fact is owned by the
    conjunction of its sources. The bookkeeping costs about 7% of DB size, 2–3%
    of indexing time and under 10% on typical queries, and heavy search
    queries run up to about 3× slower
    ([Glean blog](https://github.com/facebookincubator/Glean/blob/b0a7e677b22d621fc0dd5c2275ba078eda6a3ebd/glean/website/blog/2022-12-01-incremental.md)).
    Glean still leaves the question of *which* files to re-index to the
    caller.
  - Zoekt tombstones changed paths in older shards and gives up (a full
    rebuild) past a shard-count threshold.
  - Graphify re-extracts changed files against a read-only view of the
    unchanged ones and keeps old edges only while both endpoints survive.
  - Salsa and rust-analyzer track dependencies and stop recomputing when an
    intermediate result comes out unchanged ("backdating"). rust-analyzer's
    rule is that a change inside a function body never invalidates global
    derived data.
- **No SCIP indexer checked exposes an incremental mode.**
  - `rust-analyzer scip`, scip-typescript, scip-go and scip-python are
    whole-project batch CLIs (§3.6).
  - The SCIP *format* was designed so that "adding file-level incrementality
    should be easy": documents are self-contained and IDs are strings, not
    integers
    ([SCIP DESIGN.md](https://github.com/scip-code/scip/blob/8fe503757e133d98312b059b89e0ad5f72852d53/docs/DESIGN.md)).
  - In practice, the precise pass can be made cheaper only by:
    - running fewer or smaller builds: per-project partitioning, skipping
      unchanged projects, build caches;
    - diffing what an index produced before publishing it;
    - running precise builds less often and translating positions in between.
- **Yggdrasil today rebuilds everything on every index job.** The RFC
  describes a changed-files syntactic pass and a `git log` delta (RFC 0001 §4),
  but the code does neither (§2):
  - it runs `git archive` on the full tree;
  - it parses every file;
  - it runs `git log` over the full history;
  - it resolves every name repo-wide;
  - it builds a fresh `graph.sqlite` and tantivy archive.
- **Yggdrasil's own heuristic resolver has a global dependency.** Unqualified
  names fall back repo-wide, and N candidates get 0.9/N confidence each
  (ADR 0006). Adding a same-named declaration *anywhere* in the repo can
  therefore change edges and confidences on unchanged callers. Any syntactic
  incrementality must invalidate by *name*, not just by file.
- **For stable IDs and fingerprints, incremental reuse is a strong argument
  for the ADR 0007 constraints:**
  - A reused per-file segment is valid only if its node IDs depend on nothing
    outside that file.
  - The current `sym:<path>#<name>~<ordinal>` form is file-local, which suits
    reuse. But it is path-dependent: a move renames every Symbol in the file.
  - SCIP versions are unstable. scip-python defaults the package version to
    the current git revision, which confirms that a raw moniker cannot be the
    ID.
  - A per-node fingerprint doubles as an early-cutoff signal. This works best
    if the fingerprint is split into a *body* hash (used for Memory staleness)
    and an *interface* hash (used to invalidate dependents). The split is
    inferred, not observed in any surveyed system.

## 2. Yggdrasil today (verified)

What an index job does at `c4bec0c`:

1. **Materialize the full tree.** The worker runs `git archive` of the job's
   commit into a scratch directory (`crates/yg-index/src/worker.rs`,
   `prepare` → `extract_tree`).
2. **Walk the full history.** `git log <commit> --no-renames --name-only`
   runs over the commit's entire ancestry on every job
   (`crates/yg-index/src/history.rs`, `git_log_history`).
3. **Parse every file.** `syntactic_pass` walks every path, tree-sitter-parses
   every file in a supported language, and collects per-file facts. The
   doc comment says phase 2 "resolves the facts repo-wide; it cannot run until
   every file is parsed" (`crates/yg-index/src/pass/mod.rs`). Parsing is fresh
   each time: `parser.parse(source, None)` passes no old tree, so tree-sitter
   reuse is not in play.
4. **Resolve repo-wide by name.** `SimpleSymbolIndex::resolve` looks a name up
   in the same file, then the same directory, then falls back to *every*
   declaration of that name in the language
   (`crates/yg-index/src/resolve.rs`). Go resolution follows ADR 0006's
   package-first, then repo-wide, rule.
5. **Build and publish one immutable Shard.** The job writes the whole
   `graph.sqlite` and tantivy archive under `shards/<repo-id>/<revision>/`.
   The revision is `<commit>-syntactic-v<SCHEMA_VERSION>`, which is
   deterministic on (commit, pass, schema) (`crates/yg-shard/src/lib.rs`,
   `syntactic_revision`). The only reuse is of an *identical* revision that
   is already published (`crates/yg-index/src/shard_lifecycle.rs`,
   `prepare_or_reuse_published`). GC deletes a superseded revision's whole
   prefix (`delete_revision_objects`).
6. **Mint path-based Symbol IDs.** IDs are `sym:<path>#<name>`, with `~n` for
   the n-th same-named declaration in a file (`Node::symbol`). The FTS archive
   is not byte-reproducible, because tantivy stamps random segment IDs
   (`crates/yg-shard/src/fts.rs` header comment).
7. **No fingerprints yet.** No per-node fingerprint exists at this commit;
   RFC 0001 §5 schedules fingerprints and stable cross-pass Symbol IDs for the
   M1 schema bump.

Targets this must meet (PRD "Targets"; RFC 0001 §11 "Closed: Shard schema
evolution"):

- Syntactic freshness at p50 ≤ 8 min on the poll path and ≤ 3 min on the
  webhook path.
- The precise layer "when its build lands", and in the same hour per RFC 0001
  §4.
- A full-org re-index overnight with about 40 workers. That is about 5000
  repos × 4 worker-minutes ≈ 333 worker-hours, or about 8.3 wall-clock hours
  at 40 workers.

The 4-minute average assumes the precise pass with cached dependencies. No
phase-level timing of the current pipeline is published, so how much of a job
goes to archive, history, parse, resolve, segment build and upload is
**unmeasured** (§7).

**Gap between the documents and the code.** RFC 0001 §4 says Pass 1 parses
"changed files (full parse on first index)" and uses a "`git log` delta", but
the code does a full parse and a full log on every job. This note treats the
RFC sentence as intent, not as shipped behaviour.

## 3. Techniques

### 3.1 Per-file content-addressed caches in front of a full rebuild

**What it is.** The system keys each file's extraction output by a hash of the
file's content plus everything else that affects the output, and replays cached
outputs for unchanged files. The cross-file phase still runs over the whole
corpus.

- **Graphify:**
  - **Cache key.** `file_hash` is "SHA256 of file contents + path relative to
    root". A stat (size and mtime) fast path skips re-reading, and Markdown
    hashes only the body below the YAML frontmatter.
  - **Cache namespace.** The AST cache is namespaced by "package version,
    cache-key schema and tree-sitter grammar versions
    (`cache/ast/v{version}-s{schema}-g{grammars}/`)". The comment explains
    why: "keying purely on file content means extractor fixes shipped in a new
    release keep serving stale pre-fix results." The schema constant is at
    `_AST_CACHE_SCHEMA = 51`, and each bump lists the extractor fixes that
    forced it
    ([graphify/cache.py](https://github.com/Graphify-Labs/graphify/blob/3671f31124fdb1de4ec4e485b1bcd7059726a5f3/graphify/cache.py)).
    At this scale, cache-key discipline is a recurring cost.
  - **Cross-file phase.** Extraction is "two-pass": per-file structural
    extraction, then cross-file import resolution. An incremental rebuild
    passes the unchanged corpus as read-only `resolution_context_nodes`/`edges`
    "so a changed caller can still bind … to an unchanged callee". Only edges
    "sourced by the re-extracted files are emitted"
    ([graphify/extract.py](https://github.com/Graphify-Labs/graphify/blob/3671f31124fdb1de4ec4e485b1bcd7059726a5f3/graphify/extract.py),
    `extract`).
  - **Merge.** Preserved edges survive only if both endpoints still exist and
    the edge's source file was not re-extracted or deleted
    ([graphify/watch.py](https://github.com/Graphify-Labs/graphify/blob/3671f31124fdb1de4ec4e485b1bcd7059726a5f3/graphify/watch.py),
    the reconcile step).
  - **Consequence (inferred from that contract, not tested).** An *unchanged*
    caller is never re-resolved. A new same-named callee added elsewhere does
    not gain edges from old call sites until those files change.
- **stack-graphs (GitHub, archived 2025):**
  - **Cache.** The CLI computes `sha1(source)` as a per-file tag and skips a
    file whose stored tag matches ("cached index")
    ([tree-sitter-stack-graphs/src/cli/index.rs](https://github.com/github/stack-graphs/blob/fcb7705d5b38ae13b3665a9b2c882e5a97243d44/tree-sitter-stack-graphs/src/cli/index.rs)).
  - **Storage.** A SQLite database with a `graphs(file PRIMARY KEY, tag,
    error, value)` table plus per-file `file_paths` and `root_paths` tables
    ([stack-graphs/src/storage.rs](https://github.com/github/stack-graphs/blob/fcb7705d5b38ae13b3665a9b2c882e5a97243d44/stack-graphs/src/storage.rs)).
  - **Status.** The repository is archived and "no longer supported or updated
    by GitHub" (README).
- **Kythe:**
  - **Layout.** A `.kzip` stores each compilation unit under `units/` named by
    the "hex-coded SHA256 of record content", and each input file under
    `files/` named by the SHA256 of its content. "This organization separates
    the compilation unit descriptions from their file data, which are shared
    among multiple compilations"
    ([kythe-kzip.txt](https://github.com/kythe/kythe/blob/69141f022689a611e8a4a1d9b08a3783a2e8a9ed/kythe/docs/kythe-kzip.txt)).
  - **What that enables (inferred).** Because a unit record is addressed by
    its own digest, an output cache keyed by unit digest is possible. I did
    not find Kythe documentation of such a cache (§7).
- **Bazel's remote cache:**
  - **Layout.** An action cache ("a map of action hashes to action result
    metadata") and a content-addressable store of outputs. "Each action has
    inputs, output names, a command line, and environment variables"
    ([Bazel remote caching](https://bazel.build/remote/caching)).
  - **Caveat.** The same page warns that tools outside the workspace are not
    tracked: different compilers can yield "the same action hash" with
    different outputs. Reuse is only as sound as the key is complete.
- **Git blob IDs.** Git already content-addresses every blob: it hashes a
  `blob <size>\0` header plus the content
  ([Pro Git, Git Objects](https://git-scm.com/book/en/v2/Git-Internals-Git-Objects)).
  A worker can therefore get a per-file content key from `git ls-tree` without
  reading the file (inference from the formula).

**Layout implied.** The published artifact is unchanged: one full Shard per
revision. A side store holds per-blob extraction facts, keyed by (blob ID,
extractor version, grammar versions, schema version).

**Cross-file invalidation.** Correct by construction if the cross-file phase
reruns over all facts. The cost is that the phase stays O(repo).

**SCIP interaction.** None directly. SCIP output is per project build, not per
file (§3.6).

**IDs and fingerprints.**

- A cached per-file fact set is reusable only if the node IDs it contains are
  a function of that file alone. `sym:<path>#<name>~n` qualifies; an ID that
  embedded a repo-wide counter would not.
- A fingerprint computed from the blob's own parse can be cached with the
  facts.

### 3.2 Dependency-tracked recomputation with early cutoff

**Salsa (rust-analyzer's engine):**

- **Change tracking.** Each input carries the revision in which it last
  changed. A tracked function re-executes only if an input it read may have
  changed.
- **Backdating.** "If it's the same as last time, we can *backdate* the
  result, meaning that we say that, even though the inputs changed, the output
  didn't" ([Salsa book: the red-green algorithm](https://github.com/salsa-rs/salsa/blob/2d0a5bc65d24e0393a3c791cbe4d349d5271d6ab/book/src/reference/algorithm.md)).
  Downstream work is skipped when an intermediate value comes out unchanged,
  for example adding a comment yields the same AST.
- **Durability.** This lets functions that depend only on rarely changing
  inputs (crates.io dependencies, the standard library) skip dependency
  traversal entirely
  ([Salsa book: durability](https://github.com/salsa-rs/salsa/blob/2d0a5bc65d24e0393a3c791cbe4d349d5271d6ab/book/src/reference/durability.md)).

**rust-analyzer:**

- **Firewall.** It builds a firewall into its IR: "`ItemTree` condenses a
  single `SyntaxTree` into a 'summary' data structure, which is stable over
  modifications to function bodies".
- **Invariant.** "typing inside a function's body never invalidates global
  derived data … if you change the body of `foo`, all facts about `bar`
  should remain intact"
  ([rust-analyzer architecture](https://github.com/rust-lang/rust-analyzer/blob/37721d141370e0904033f7fc3a3abe11c446c4c1/docs/book/src/contributing/architecture.md)).

**Tree-sitter.** Incremental *parsing* is the same idea at the parse level:
`ts_tree_edit` plus re-parsing with the old tree "will create a new tree that
internally shares structure with the old tree"
([tree-sitter: Advanced Parsing](https://github.com/tree-sitter/tree-sitter/blob/32594ac6e0532a31bcca7fcd043c34ff5e5e007a/docs/src/using-parsers/3-advanced-parsing.md)).
`ts_tree_get_changed_ranges` reports where the hierarchical structure changed
([api.h](https://github.com/tree-sitter/tree-sitter/blob/32594ac6e0532a31bcca7fcd043c34ff5e5e007a/lib/include/tree_sitter/api.h)).

- **The catch for a server.** This needs the previous tree in memory and an
  edit script. A batch indexer that materializes a commit and parses from
  bytes has neither, so its practical unit is the whole file: skip unchanged
  blobs.
- **Possible use (inferred).** Diffing a changed file's old and new trees
  could narrow *which declarations* changed, and so which fingerprints to
  recompute.

**Layout implied.**

- **In memory.** A long-lived process holding the dependency graph.
- **Persisted.** A per-unit *interface summary*: the declared names and
  signatures a file exports. Its hash gates re-resolution of the files that
  import or reference it.

**Cross-file invalidation.** Explicit. Dependents are recomputed only when an
input they read changed in value, not merely in revision.

**SCIP interaction.**

- SCIP indexers do not expose salsa-style persistence. `rust-analyzer scip`
  takes a path, `--output`, `--config-path`, `--exclude-vendored-libraries`
  and `--num-threads`, and no cache or incremental option
  ([flags.rs](https://github.com/rust-lang/rust-analyzer/blob/37721d141370e0904033f7fc3a3abe11c446c4c1/crates/rust-analyzer/src/cli/flags.rs)).
  Each run loads the workspace fresh (`load_workspace_at` in
  [scip.rs](https://github.com/rust-lang/rust-analyzer/blob/37721d141370e0904033f7fc3a3abe11c446c4c1/crates/rust-analyzer/src/cli/scip.rs)).
- Salsa's savings would require a warm, long-lived indexer per repo. I found
  no SCIP CLI that supports that.

**IDs and fingerprints.** The interface/body split maps directly onto
fingerprints (§4.5).

### 3.3 Stacked databases with unit ownership (Meta Glean)

**Mechanism** (all quotes from Simon Marlow,
[Incremental indexing with Glean](https://github.com/facebookincubator/Glean/blob/b0a7e677b22d621fc0dd5c2275ba078eda6a3ebd/glean/website/blog/2022-12-01-incremental.md),
2022-12-01, and the
[implementation notes](https://github.com/facebookincubator/Glean/blob/b0a7e677b22d621fc0dd5c2275ba078eda6a3ebd/glean/website/docs/implementation/incrementality.md)):

- **Hiding and stacking.** "To produce a modified DB, we hide a portion of the
  data in the original DB, and then stack a DB with the new data on top." The
  command is `glean create --repo <new> --incremental <old> --exclude A,B,C`.
- **Concurrent versions.** The stack is invisible to queries, and "old" stays
  usable at the same time. "These DB stacks form a tree … no data is being
  modified."
- **Units.** Facts are labelled with a *unit*: "a unit is just a string …
  typically a unit might be a filename or module name". The implementation
  notes recommend "one unit per file or module".
- **No dangling references.** Hiding must leave the DB valid ("no dangling
  references"). After writing, Glean "propagates all the units through the
  graph of facts, resulting in a mapping from facts to ownership sets". If
  fact *x* (unit A) refers to *y* (unit B), *y*'s owner becomes `A || B`, so
  hiding B keeps *y* visible.
- **Propagation cost.** Propagation is O(facts) time and, currently, O(facts)
  space. It runs as `glean complete` after writes and before derivation
  (implementation notes).
- **Derived facts.** A derived fact "is visible … when all of the facts that
  it was derived from are visible", so its owner is the conjunction
  `{P} & {Q}`.
- **Across the stack.** "We have to transitively propagate ownership to facts
  in the base DB(s), store that in 'new'", and "there must be a single
  namespace of ownership sets for the whole DB stack."
- **Storage.** Ownership sets are Elias-Fano coded. The fact→set map is an
  interval map, because "facts produced together tend to have the same
  ownership set". There are "typically … between 10-100x more facts than
  sets".

**Reported costs (blog):**

| Cost | Reported |
|---|---|
| DB size | about +7% |
| Indexing time | +2–3% for Python, "in the noise" for Hack |
| Query time | under 10% for "typical" queries after caching, but "queries that do a lot of searching may be impacted by around 3x" |

**Stated limits:**

- Incremental derivation is implemented only "for some kinds of query".
  Making it general "is a hard problem" (blog).
- A base fact that gains a new owner in a stacked DB can be visible while
  facts derived from it earlier are not. The fix, incremental derivation on
  the stacked DB, "isn't implemented yet" (implementation notes).

**What Glean does not do.** It does not decide which units to re-index: "we
would first decide which files need to be reindexed, create an incremental DB
that hides those files from the base DB and then add the new facts" (blog).
Picking dependents is the indexer's job. That is the same problem as §3.2 and
§4.2.

**Derived predicates.** Glean separates `stored` derived predicates
(materialized by `glean derive`, which runs "*before* using `glean finish`")
from on-demand ones computed at query time
([derived.md](https://github.com/facebookincubator/Glean/blob/b0a7e677b22d621fc0dd5c2275ba078eda6a3ebd/glean/website/docs/derived.md)).
This is the same trade as materializing reverse edges in a Shard versus
computing them per Verb.

**Layout implied.**

- **Storage.** A base Shard plus a chain or tree of increments. Each node and
  edge row carries an ownership-set ID, and each layer carries the set of
  hidden units.
- **Reads.** Every read filters through a visibility slice.
- **Compaction.** Rebuilding a fresh base is needed to bound the stack. Glean
  docs do not prescribe a compaction policy (§7).

**SCIP interaction.**

- A precise run that re-emits only changed documents could be written as an
  increment that hides those documents' units.
- Because indexers re-emit everything anyway (§3.6), the saving is in storage
  and publication, not in build compute.
- Glean ingests SCIP for some languages: it has a `python-scip` indexer page
  in its docs
  ([website docs tree](https://github.com/facebookincubator/Glean/tree/b0a7e677b22d621fc0dd5c2275ba078eda6a3ebd/glean/website/docs/indexer)).
  I did not check whether that path is incremental (§7).

### 3.4 Delta shards with tombstones (Zoekt) and segment deletes (tantivy)

**Zoekt.** Zoekt is the per-repo trigram index that RFC 0001 cites as a
pattern. It supports *delta builds*
([index/builder.go](https://github.com/sourcegraph/zoekt/blob/b55ce727df8cfbed76b8a1420445fd0cf93f3256/index/builder.go)):

- **Mechanism:**
  - `IsDelta` means "this run contains only the changed documents since the
    last run".
  - `changedOrRemovedFiles` "will be tombstoned in the older shards for this
    repository".
  - Delta shards are numbered after the existing ones "so that all the shards
    [can] be discovered as a set".
  - "Non-delta shard builds delete all existing shards before they write out
    new ones … delta shard builds work by stacking changes on top of existing
    shards."
- **Guards that force a full build:**
  - A changed branch set (`deltaBranchSetError`).
  - Changed index options (`deltaIndexOptionsMismatchError`).
  - Compound shards and submodules, which are unsupported.
  - A configurable `DeltaShardNumberFallbackThreshold`: past that many
    existing shards, the builder errors and the caller falls back to a normal
    build
    ([gitindex/index.go](https://github.com/sourcegraph/zoekt/blob/b55ce727df8cfbed76b8a1420445fd0cf93f3256/gitindex/index.go)).
- **What this is.** A bounded-depth overlay with full-rebuild compaction.

**tantivy.** tantivy, Yggdrasil's FTS engine, works this way natively
([ARCHITECTURE.md](https://github.com/quickwit-oss/tantivy/blob/0aa896fe29417107ad0bbab502c1a9fd1077f272/ARCHITECTURE.md)):

- "A tantivy index is a collection of smaller independent immutable segments."
- Deletes are by term and recorded in immutable per-commit "alive bitset"
  files.
- Merges "get rid of tombstoned documents" and keep the segment count down,
  since "having hundreds can have a measurable impact on the search
  performance".
- Yggdrasil currently packs one tantivy index per Shard into a single
  archive, so it does not use this.

**Layout implied.** Per revision: the previous revision's segments, plus a
delta segment for changed files, plus a tombstone list of paths superseded in
older segments. Periodic compaction writes a fresh base.

**Graph segment.** The graph segment is SQLite.

- **Reading layered segments.** It can read layered files with `ATTACH`, but
  "the number of simultaneously attached databases is limited to
  SQLITE_MAX_ATTACHED which is set to 10 by default", and "cannot be
  increased above 125" ([SQLite limits](https://www.sqlite.org/limits.html)).
- **Consequence.** That caps how deep an attached overlay can be. Deeper
  layering would need application-level merging.

**Cross-file invalidation.**

- Tombstoning works cleanly for *file-owned* rows: text, File nodes, a file's
  own Symbols.
- An edge stored with its source file (a CALLS edge from `b.go` into `a.go`)
  is not refreshed when only `a.go` changes. It must be re-resolved and
  re-emitted, or filtered when its destination is tombstoned. Otherwise the
  overlay serves a dangling or stale edge.
- Zoekt has no edges, so it never faced this. Glean's ownership propagation
  (§3.3) is the general answer.

### 3.5 Query-time stitching: keep per-file results, resolve at read

**Stack graphs.** "We look at each file completely in isolation", extract
per-file facts, and "bring together the data from all files in the commit that
you're looking at" at query time. That avoids having to "reanalyze every file
in the repository" when one changes
([Introducing stack graphs](https://github.blog/open-source/introducing-stack-graphs/),
Douglas Creager, 2021-12-09). The README calls the approach "efficient,
incremental" and independent of build tools. It is archived (§3.1).

**Kythe:**

- **Graph store.** "Combining two graph stores into one is accomplished by
  computing the set union of the two stores … a simple
  merge-and-remove-duplicates"
  ([kythe-storage.txt](https://github.com/kythe/kythe/blob/69141f022689a611e8a4a1d9b08a3783a2e8a9ed/kythe/docs/kythe-storage.txt)).
  Per-unit outputs compose by union because nodes are named by
  analyzer-generated *VNames* (signature, corpus, root, path, language), not
  by integer IDs.
- **Serving tables.** Serving tables are a separate bulk step: `write_tables`
  "creates a combined xrefs/filetree/search serving table based on a given
  GraphStore" on Apache Beam
  ([write_tables.go](https://github.com/kythe/kythe/blob/69141f022689a611e8a4a1d9b08a3783a2e8a9ed/kythe/go/serving/tools/write_tables/write_tables.go)).
  So the documented open-source path batch-recomputes reverse edges.

**SCIP's own stance.** SCIP takes the same position:

- It is "a *transmission* format … not … a *storage* format for querying".
- It avoids direct graph encoding.
- It records no bidirectional links, because "code navigation fundamentally
  requires some form of bidirectional lookup which is best served by a query
  engine" (SCIP DESIGN.md).
- Symbols are strings of the form `<scheme> <manager> <package-name>
  <version> <descriptor>+`, and "local" symbols are document-scoped
  ([scip.proto](https://github.com/scip-code/scip/blob/8fe503757e133d98312b059b89e0ad5f72852d53/scip.proto)).
- A reference in one document to a symbol defined in another is joined by
  string equality at read time, so there is nothing to invalidate at write
  time.

**Layout implied.** Per-file forward facts only, with reverse indexes either
computed at query time or rebuilt in a cheap batch step.

**Costs.** Query-time work grows with the number of files consulted. Precision
is bounded by what per-file analysis can know: stack graphs are syntactic, and
the archived implementation shows the maintenance risk.

### 3.6 Partitioning precise builds by project or compilation unit

**Sub-project partitioning.** This is the only incrementality available to a
whole-project SCIP indexer.

- **Sourcegraph.** It stores uploads per *indexing root* and indexer. "There
  is a nearest upload *per distinct indexing root directory*", and uploads are
  keyed by root and indexer (later a hash of both)
  ([Optimizing a code intelligence commit graph, part 2](https://sourcegraph.com/blog/optimizing-a-code-intelligence-commit-graph-part-2),
  Eric Fritz, 2021-06-03).
- **Auto-indexing inference.** It creates one job per `go.mod` directory and
  one per `tsconfig.json` directory
  ([Sourcegraph auto-indexing](https://sourcegraph.com/docs/code-navigation/auto-indexing)).
- **Implication.** A monorepo change re-indexes only the roots it touches,
  plus the roots that depend on them.

**Indexer-level narrowing:**

| Indexer | Narrowing option |
|---|---|
| scip-go | Takes package patterns, defaulting to `./...` ([main.go](https://github.com/scip-code/scip-go/blob/aedacd940b9c42a4885b5ff4c068598dddbb9c1d/cmd/scip-go/main.go)) |
| scip-python | `--target-only <path>` "limit analysis to the following path" ([MainCommand.ts](https://github.com/sourcegraph/scip-python/blob/51dd05f5c9d43d87471c2b3bf9f74a9256b627c7/packages/pyright-scip/src/MainCommand.ts)) |
| scip-typescript | Takes a project list and workspace flags, and has a `--no-global-caches` switch "to disable global caches between TypeScript projects" ([CommandLineOptions.ts](https://github.com/sourcegraph/scip-typescript/blob/7f51391d9770c63be2449463c8eb89f98c9e1468/src/CommandLineOptions.ts)) |

None of these four CLIs advertises an incremental or resume-from-previous-index
option. I did not check scip-java, scip-clang, scip-ruby or scip-dotnet
exhaustively (§7).

**Build-graph invalidation.**

- **Dependents.** Bazel's `rdeps(u, x)` "evaluates to the reverse dependencies
  of the argument set x within the transitive closure of the universe set u"
  ([Bazel query language](https://bazel.build/query/language)). This is the
  standard way to turn "files changed" into "units to rebuild".
- **Without Bazel.** For repos without Bazel, the analogue is the
  manifest-level dependency graph: go.mod modules, Cargo workspace members,
  tsconfig project references. This is an inference; mapping each ecosystem
  is unscoped work.

**Kythe's per-unit model and claiming:**

- **Per-unit analysis.** Kythe analyzes per compilation unit, replayed from
  the kzip.
- **Redundancy.** Separate analyses over each C++ translation unit "may lead
  to redundant work … especially when … there is a large overlap in the set of
  headers that each translation unit includes".
- **Claiming.** Its *claiming* scheme assigns each (header, preprocessor
  transcript) pair to a single TU so the work is done once
  ([claiming.md](https://github.com/kythe/kythe/blob/69141f022689a611e8a4a1d9b08a3783a2e8a9ed/kythe/cxx/indexer/cxx/claiming.md)).
- **Relevance.** This matters if scip-clang lands (RFC 0001 §4, M2+).

**SCIP interaction.**

- Partitioning works with SCIP as-is. The cost is one build per affected
  root, plus the dependency-cache strategy that is still open (RFC 0001 §11
  Q3).
- Correctness depends on finding dependents. A changed exported symbol in root
  R invalidates references in every root that imports R, including other repos
  (§4.3).

### 3.7 Not re-indexing: serve the nearest index and translate positions

**Sourcegraph.** Sourcegraph's precise navigation does not need an index at
every commit.

- **Nearest upload.** The commit graph table stores, per commit, "the source
  commit, the identifier of the visible upload, and the distance". Commits
  without a row use "the visible uploads attached to the ancestor" (commit
  graph blog, §3.6).
- **Position translation.** Positions are translated across the diff by a
  `GitTreeTranslator`. "TranslatePosition returns None if the given position
  is on a line that was removed or modified between from and to"
  ([gittree_translator.go](https://github.com/sourcegraph/sourcegraph-public-snapshot/blob/c864f15af264f0f456a6d8a83290b5c940715349/internal/codeintel/codenav/gittree_translator.go),
  Sourcegraph public snapshot, last commit 2024-08-22). So results on edited
  lines are dropped, not guessed.
- **Fallback.** Search-based navigation covers the gap: "Search-based Code
  Navigation is used as a fallback when precise navigation is not available"
  ([Precise code navigation](https://sourcegraph.com/docs/code-navigation/precise-code-navigation)).
- **Scheduling.** Auto-indexing reconsiders a repository at most every
  `PRECISE_CODE_INTEL_AUTO_INDEXING_REPOSITORY_PROCESS_DELAY`, default 24 hours
  (auto-indexing docs, read via a summarizer; exact wording **unverified**,
  §7).

**Layout implied.** The latest precise Shard revision is kept beside a newer
syntactic one, and the read path maps precise positions through `git diff`
hunks between the two commits.

**Cross-file invalidation.** None at write time. Staleness is handled per line
at read time: an unchanged line in an unchanged file keeps its precise edge
even if the callee moved elsewhere. The callee's *location* may be stale, but
its identity is stable if Symbol IDs are.

**SCIP interaction.**

- This is the most direct way to meet a "precise layer when its build lands"
  target with fewer builds.
- Precise runs can be coalesced, one per N commits or per time window.
- Syntactic Shards carry freshness in between.

## 4. Cross-cutting analysis

### 4.1 Storage layouts compared

| Layout | Seen in | Publish cost per change | Read-path cost | Cross-file edge handling | Compaction |
|---|---|---|---|---|---|
| Full immutable Shard per revision (today) | Yggdrasil, Zoekt non-delta | O(repo) build + upload | None extra | Recomputed every time | N/A |
| Full Shard + per-blob fact cache | Graphify, stack-graphs, Bazel AC/CAS | O(changed) parse; O(repo) resolve, build, upload | None extra | Recomputed every time (if resolve reruns) | Cache eviction only |
| Content-addressed per-file segments, manifest lists them | Kythe kzip files (input side), Bazel CAS | O(changed) segments + manifest | Merge many segments per Verb (SQLite ATTACH ≤ 10 by default, 125 maximum) | Edges must be owned by a unit and re-emitted when invalidated | Segment GC by reference count across revisions |
| Delta/overlay with tombstones | Zoekt delta, tantivy segments | O(changed) + tombstone list | Tombstone filter on every read; depth-bounded | Destination-side changes leave stale source-owned edges unless re-emitted | Full rebuild past a threshold |
| Stacked DB with ownership sets | Glean | O(changed) + ownership propagation | Visibility check per fact (<10% typical, ~3× search) | Ownership propagation keeps references valid; picking dependents is the caller's job | Not documented |
| Per-file facts, query-time join | Stack graphs, SCIP symbols, Kythe union | O(changed) | Resolution or join at query time | No write-time invalidation | N/A |
| Nearest index + diff translation | Sourcegraph precise | Zero for skipped commits | Diff per (file, commit pair), cacheable | Per-line drop at read | N/A |

### 4.2 Cross-file invalidation in Yggdrasil's own syntactic resolver

A per-file cache can only make the resolver incremental if the resolver's
inputs per call site are known. From `resolve.rs` and ADR 0006, a call site's
edges depend on:

- **Its own file's facts.** These are already per-file.
- **The candidate set for its name:**
  - same file, then same directory (≈ Go package), then **every declaration of
    that name in the repo** for unqualified simple-language names and Go
    methods;
  - for Go package-qualified names, the declarations in the import's
    directories, mapped through go.mod module paths.
- **The candidate count N.** It sets confidence to 0.9/N.

So the invalidation key for a call site is (its file's blob) × (the candidate
sets for the names it uses at each scope level). A change that adds, removes or
renames a declaration of name `n` invalidates every call site that resolves `n`
at the repo-wide level, in any file. A change to a function *body* that leaves
declarations alone invalidates nothing beyond its own file. That is
rust-analyzer's invariant (§3.2), and with name-keyed resolution it holds
without type information.

This suggests a concrete early-cutoff summary per file: the multiset of
(language, scope, name, kind) declarations it contributes. When a file's
summary hash is unchanged, no other file needs re-resolution. Inference: this
is not measured. The fraction of commits that change only bodies is an open
empirical question (§7).

Go module mapping adds one more global input. A change to any `go.mod`
re-maps import directories, so it should invalidate every Go file's imports
(the most-specific-module rule in ADR 0006).

### 4.3 Cross-repo edges

Yggdrasil already avoids materializing cross-repo symbol edges in Shards:

- workers emit `provides`/`depends` package facts;
- the control plane resolves them into `xrepo_edges` (repo → repo via a
  package);
- SCIP monikers are "the global join key" (RFC 0001 §2, §4).

That is the late-binding pattern of §3.5 at repo granularity. When repo X
publishes a revision, only X's package facts change, and the repo-level
`xrepo_edges` rows recompute from them. No other repo's Shard needs rewriting.

Two things do need invalidation once precise `callers`/`impact` exist:

- **Materialized reverse edges.** A cross-repo reverse index, such as "callers
  in Y of symbol S in X" stored somewhere, needs rdeps-style invalidation when
  S changes or disappears. Glean's cross-DB ownership propagation (§3.3) is the
  heavyweight form; recomputing at query time from monikers through
  `xrepo_edges` fan-out is the lightweight one (inference).
- **Memories anchored in X.** These are already handled: staleness is checked
  against the new revision in the pointer-swap transaction (RFC 0001 §5.1). A
  delta-aware publish would know the changed node set directly. That would
  make the check O(changed nodes) instead of O(anchors in the repo), an
  inference that depends on the layout chosen.

Version skew is the cross-repo trap:

- a SCIP symbol embeds `<version>`;
- scip-python's `--project-version` "defaults to the current git revision"
  (scip-python `MainCommand.ts`);
- so an un-normalized moniker from X changes on every commit, and a join keyed
  on it would break every caller in Y each time.

ADR 0007 and RFC 0001 §5 already make the versioned moniker an attribute, not
the ID. Incrementality adds a reason: the join key must survive revisions that
were *not* re-indexed on the other side.

### 4.4 What incrementality is and is not available for SCIP precise passes

- **Not available: incremental indexer runs.** The four indexers whose CLIs I
  checked run the whole project each time (§3.2, §3.6). The type-checker or
  compiler front-end is the cost, and only build or dependency caches (RFC
  0001 §11 Q3) reduce it.
- **Available: smaller runs.**
  - Per-root partitioning, with roots inferred from manifests, as Sourcegraph
    does.
  - Package or path narrowing where an indexer supports it (scip-go patterns,
    scip-python `--target-only`).
  - Skipping a root whose input hash is unchanged. The hash covers the
    source-file blob IDs, lockfiles, toolchain version and indexer version.
    Bazel's warning applies: an incomplete key gives wrong hits.
- **Available: smaller publications.**
  - SCIP `Document`s are self-contained by design, keyed by `relative_path`
    with document-scoped `local` symbols. A worker can therefore diff a fresh
    index against the previous revision's per-document digests and write only
    changed documents into a delta, overlay or per-file segment.
  - The format's design goal ("adding file-level incrementality should be
    easy") supports this. That indexers produce byte-identical documents for
    unchanged files across runs is **unverified** (§7).
- **Available: fewer runs.** Coalescing plus nearest-precise-revision serving
  with diff translation (§3.7).
- **Cross-file correctness under partitioning.** A changed exported symbol in
  root R makes every importing root stale. The precise layer for those roots
  is stale until they re-run, or until positions are translated and per-line
  drops applied.

### 4.5 Stable Symbol IDs and fingerprints under incremental indexing

Every reuse technique above (cached facts, overlays, ownership hiding,
query-time joins, nearest-index serving) assumes that an *unchanged* entity has
the *same ID* in the old and new data. Otherwise:

- edges from reused units dangle;
- Anchors read as `removed`;
- Glean-style ownership cannot hide the right rows.

Concretely:

1. **IDs must be a function of the owning unit.** A cached or overlaid unit is
   reusable only if its IDs depend on nothing outside it.
   `sym:<path>#<name>~n` meets this today. Any future ID scheme that consults
   repo-wide state, such as a global disambiguation counter or a resolution
   result, breaks per-file reuse.
2. **Path-dependence trades reuse against churn:**
   - Path-based IDs make a file move or rename a delete plus an add of every
     Symbol in it.
   - SCIP descriptors are package- and namespace-qualified, which avoids this
     where the language's identity is not path-derived. That holds for Go
     packages, but per-indexer behaviour varies, for example Python modules
     derive from file paths (**unverified**, §7).
   - The ordinal suffix `~n` makes IDs depend on declaration order within a
     file. Inserting an earlier same-named declaration renumbers later ones.
3. **The cross-pass stability rule is the same rule.** RFC 0001 §5 requires
   the syntactic and precise passes to agree on IDs and fingerprints for the
   same blob. Under incremental indexing, the precise layer often lags the
   syntactic one (§3.7). That rule is what lets the system join a precise
   edge from revision *r₁* to a syntactic node at *r₂*.
4. **Fingerprints have two roles, which may want two hashes:**
   - **Body fingerprint.** This is the hash of the normalized defining span,
     as RFC 0001 §5 specifies. It is what Memory staleness wants: any change
     to what the Memory describes flags it.
   - **Interface fingerprint.** This covers the declared name, kind, scope
     and, where available, signature. It is what dependency invalidation
     wants: salsa's backdating and rust-analyzer's `ItemTree` firewall cut off
     at the interface.
   - **Why one hash is not enough.** Using the body fingerprint to invalidate
     dependents would re-resolve callers on every body edit. Using the
     interface fingerprint for Memories would miss behaviour changes.
   - This split is an inference from §3.2; no surveyed code graph documents
     it.
5. **Fingerprints are cacheable with the unit.** Computed from the blob's own
   parse, a fingerprint can be stored with the per-blob facts (§3.1) and is
   identical whenever the blob ID is.
6. **Delta publication gives the staleness check its input for free.** The set
   of nodes whose fingerprint changed, or that disappeared, is exactly what an
   overlay or per-file publish already computes. The Memory staleness step in
   the pointer-swap transaction could consume it directly (inference).

## 5. Options for Yggdrasil, with costs

These options are not mutually exclusive; several stack.

The cost columns name the work they add, and the arithmetic uses the PRD
planning figures. No option has been prototyped or measured here.

| # | Option | What changes | Saves | Costs and risks |
|---|---|---|---|---|
| O1 | **Measure first; keep full rebuilds** | Add per-phase timings (archive, `git log`, parse, resolve, SQLite build, tantivy build, upload) to index jobs | Nothing directly; tells which option pays | Small instrumentation work. Defers the decision. The 4 worker-minute planning figure stays unvalidated without it. |
| O2 | **Incremental history** | `git log <prev>..<new>` delta merged with the previous revision's Commit/Contributor/TOUCHES data, as RFC 0001 §4 already describes | The full-history walk, which grows without bound with repo age | Needs the previous revision's history rows as an input: either read back from the old Shard or kept in a side store. Force-pushes and history rewrites need a full-walk fallback. Brings the code in line with the RFC. |
| O3 | **Per-blob extraction cache** (§3.1) | Workers look up per-file facts and fingerprints by (git blob ID, extractor version, grammar versions, schema version) before parsing; the cache lives worker-locally or in object storage | Parse and read time for unchanged blobs | Resolve, segment build and upload stay O(repo). Cache-key discipline is an ongoing tax (Graphify's schema counter is at 51). Object-storage cache traffic. A worker-local cache misses when a job lands on a cold worker, which the queue allows today. Shard format unchanged. |
| O4 | **Name-keyed early-cutoff resolution** (§3.2, §4.2) | Persist each file's declaration-summary hash. Re-resolve only call sites whose names' candidate sets changed. Invalidate all Go imports on any `go.mod` change. | Resolve becomes O(changed names' call sites) | Needs the previous revision's resolved edges as input, plus a reverse index from name to call sites. Subtle correctness surface: ADR 0006's spread confidence depends on global N. Needs a property test that the incremental result equals the full one. Builds on O3. |
| O5 | **Content-addressed per-file Shard segments** | A Shard becomes a manifest of segment digests; unchanged files' segments are shared across revisions | Upload and storage proportional to the change. Cross-revision dedup, and possibly across forks or vendored copies (inference). | Read path merges many segments per Verb. SQLite ATTACH (10 by default, 125 maximum) forces grouping files into larger segments or a different graph format. GC moves from deleting a revision prefix to reference counting segments across revisions. Edges need an owning unit and must be re-emitted when invalidated (§3.4). The cache tier is already keyed by checksum (RFC 0001 §6), which fits. Revision IDs stop being a pure function of the commit. |
| O6 | **Delta/overlay Shards with tombstones** (Zoekt-style) | A revision is a base plus up to K delta Shards; tombstones name superseded units; compaction past K | Publish proportional to the change; fast syntactic freshness on big repos | Every Verb filters tombstones and merges layers. Stack depth is bounded by K and by the ATTACH limit. Source-owned edges into changed destinations need re-emission or a destination-side filter. The Memory staleness transaction must read the layered view. GC must pin bases. Schema bumps (RFC 0001 §11) still force full rebuilds, as Zoekt's options-mismatch rule does. |
| O7 | **Stacked Shards with ownership sets** (Glean-style) | O6 plus per-row ownership-set IDs and propagation, so hidden units never leave dangling references | O6's savings, without stale cross-file edges | The most machinery. Glean reports about +7% size, +2–3% indexing, under 10% typical query cost and about 3× on search-heavy queries. Its derived-fact ownership across stacks is unfinished. Picking dependents is still needed (O4). |
| O8 | **Precise: per-root partitioning and skip-unchanged** (§3.6) | Infer roots per manifest. Hash each root's inputs and skip unchanged roots. Re-run dependent roots. | Precise compute proportional to affected roots, the dominant cost at scale | Per-ecosystem root and dependency inference. A partial precise layer within one repo means mixed provenance across roots. Interacts with open question Q4 (one Shard per repo or per workspace member). Correctness depends on complete input keys. |
| O9 | **Precise: ingest-side Document diff** (§4.4) | Compare each SCIP `Document` against the previous precise revision's per-document digest. Only changed ones produce new rows. | Precise publication size; pairs with O5, O6 or O7 | No compute saved: the build still runs. Requires indexer output to be deterministic per unchanged file (**unverified**). |
| O10 | **Precise: nearest-revision serving with diff translation** (§3.7) | Keep the last successful precise revision; translate its positions to the current syntactic revision via `git diff` hunks; drop results on edited lines; coalesce precise runs | Fewer precise builds. Syntactic freshness unaffected. A cleaner fit for "precise layer when its build lands". | Read-path diff computation, with a cache per (path, from, to) like Sourcegraph's hunk cache. Verbs must report which revision precise data came from. `impact` and `callers` answers can mix revisions. An anchor's fingerprint is checked against the syntactic revision while precise edges come from an older one. |
| O11 | **Cross-repo: keep late binding** (§4.3) | Cross-repo callers and impact are joined at query time on version-normalized monikers through `xrepo_edges` | No cross-repo write-time invalidation at all | Query-time fan-out cost, bounded by `xrepo_edges` (RFC 0001 §11 Q5). The alternative, materialized cross-repo reverse edges, needs rdeps-style invalidation across Shards. |

Two arithmetic anchors from the PRD figures, for comparing options. The
assumptions behind them are **unmeasured**.

- **Overnight full-org rebuild.** About 333 worker-hours. Only O8 and O10
  (precise compute) and O3 (parse) reduce the compute behind it. O5, O6 and O7
  reduce publication and storage. Schema bumps reset every incremental option
  to a full rebuild (RFC 0001 §11), so the overnight budget must hold
  regardless.
- **Syntactic p50 ≤ 8 min.** Today that budget covers a full archive, log,
  parse, resolve and build of the repo. For a monorepo where a full syntactic
  pass exceeds that budget, O2 + O3 + O4 (compute) and O5 or O6 (publish) are
  the levers. O1 decides whether any repo in the target fleet is in that
  regime.

## 6. Questions the M1 ID and fingerprint design will meet

These are questions, not answers. Per §4.5, every incremental option above
depends on how the M1 schema bump answers them. Answering them early is cheap.
Changing the answers later means a schema bump, and a schema bump means a
full-fleet rebuild (RFC 0001 §11).

- Is a Symbol ID a function of its owning file alone, never of repo-wide
  resolution state? Every reuse technique needs this.
- Do file moves change Symbol IDs (path-based churn) or not (descriptor-based
  stability), and how are same-name ordinals assigned?
- Is the fingerprint computed from the blob alone? RFC 0001 §5 implies so. If
  it is, fingerprints can be cached by blob ID.
- Is there one fingerprint per node, or a body hash plus an interface hash?
- Does a Shard record per-file digests (blob ID and fingerprint summary), so
  that a later delta or overlay publisher has a baseline to diff against
  without re-reading the old Shard's rows?

## 7. Open questions and unverified claims

- **Phase timings (unmeasured).** No per-phase timing of the current index job
  exists, so the share of archive, history, parse, resolve, build and upload
  is unknown. This is O1.
- **Body-only commit share (unmeasured).** The fraction of commits that change
  only function bodies, and so would cut off early under O4, is unknown for
  any fleet.
- **SCIP indexers not checked.** I checked the CLIs of `rust-analyzer scip`,
  scip-typescript, scip-go and scip-python for incremental or resume options
  and found none. scip-java, scip-clang, scip-ruby and scip-dotnet were not
  checked exhaustively: GitHub code search was rate-limited, and scip-ruby's
  "incremental" hits were Sorbet LSP internals.
- **SCIP determinism (unverified).** Whether SCIP indexers emit
  byte-identical `Document`s for unchanged files across runs is unverified,
  and O9 depends on it.
- **Path-derived descriptors (unverified).** Whether SCIP descriptors are
  path-independent for a given indexer, for example whether scip-python
  module descriptors derive from file paths, is unverified.
- **Glean: SCIP path and production use.** Whether Glean's SCIP-based
  indexers support incremental DBs was not checked. How Meta chooses units to
  re-index (dependents) in production is undocumented in the sources read.
  Glean documents no compaction policy for deep stacks.
- **Kythe output cache.** Whether Kythe ships an indexer-output cache keyed by
  compilation-unit digest is not established. Only the kzip naming, which
  makes one possible, is verified.
- **Sourcegraph wording.** The auto-indexing environment variable names and
  defaults (2-minute task interval, 24-hour repository process delay) come
  from a summarizing fetch of the docs page. The exact wording is unverified.
- **Sourcegraph current state.** The `GitTreeTranslator` source is from the
  public snapshot (last commit 2024-08-22). Sourcegraph's main repository has
  been closed since August 2024 (see the
  [landscape note](team-brain-landscape.md) §5.1), so current behaviour may
  differ.
- **Graphify unchanged callers (inferred).** The claim that unchanged
  Graphify callers do not re-bind to a newly added callee until they are
  re-extracted is inferred from the `extract`/reconcile contracts, not tested.
- **rust-analyzer persistence (inferred).** That `rust-analyzer scip` has no
  persistent cross-run cache is inferred from its flags and its fresh
  `load_workspace_at` per run. A persistence feature outside this subcommand
  was not ruled out.

## 8. Sources

Repository sources are pinned to the commits read on 2026-11-22/23.

**Yggdrasil (commit `c4bec0c`):**

- `crates/yg-index/src/worker.rs`
- `crates/yg-index/src/history.rs`
- `crates/yg-index/src/pass/mod.rs`
- `crates/yg-index/src/resolve.rs`
- `crates/yg-index/src/shard_lifecycle.rs`
- `crates/yg-shard/src/lib.rs`
- `crates/yg-shard/src/fts.rs`
- `docs/rfc/0001-yggdrasil-v1.md` §§2, 4, 5, 5.1, 6, 11
- `docs/PRD.md` Targets
- `docs/adr/0002`, `0005`, `0006`, `0007`
- `CONTEXT.md`

**Glean (`facebookincubator/Glean` @ `b0a7e677`):**

- [blog/2022-12-01-incremental.md](https://github.com/facebookincubator/Glean/blob/b0a7e677b22d621fc0dd5c2275ba078eda6a3ebd/glean/website/blog/2022-12-01-incremental.md)
  (also at https://glean.software/blog/incremental/)
- [docs/implementation/incrementality.md](https://github.com/facebookincubator/Glean/blob/b0a7e677b22d621fc0dd5c2275ba078eda6a3ebd/glean/website/docs/implementation/incrementality.md)
  (also at https://glean.software/docs/implementation/incrementality/)
- [docs/derived.md](https://github.com/facebookincubator/Glean/blob/b0a7e677b22d621fc0dd5c2275ba078eda6a3ebd/glean/website/docs/derived.md)

**Kythe (`kythe/kythe` @ `69141f02`):**

- [kythe-kzip.txt](https://github.com/kythe/kythe/blob/69141f022689a611e8a4a1d9b08a3783a2e8a9ed/kythe/docs/kythe-kzip.txt)
- [kythe-storage.txt](https://github.com/kythe/kythe/blob/69141f022689a611e8a4a1d9b08a3783a2e8a9ed/kythe/docs/kythe-storage.txt)
- [claiming.md](https://github.com/kythe/kythe/blob/69141f022689a611e8a4a1d9b08a3783a2e8a9ed/kythe/cxx/indexer/cxx/claiming.md)
- [write_tables.go](https://github.com/kythe/kythe/blob/69141f022689a611e8a4a1d9b08a3783a2e8a9ed/kythe/go/serving/tools/write_tables/write_tables.go)

**SCIP and indexers:**

- `scip-code/scip` @ `8fe50375`:
  [scip.proto](https://github.com/scip-code/scip/blob/8fe503757e133d98312b059b89e0ad5f72852d53/scip.proto),
  [docs/DESIGN.md](https://github.com/scip-code/scip/blob/8fe503757e133d98312b059b89e0ad5f72852d53/docs/DESIGN.md)
- `scip-code/scip-go` @ `aedacd94`:
  [cmd/scip-go/main.go](https://github.com/scip-code/scip-go/blob/aedacd940b9c42a4885b5ff4c068598dddbb9c1d/cmd/scip-go/main.go)
- `sourcegraph/scip-python` @ `51dd05f5`:
  [MainCommand.ts](https://github.com/sourcegraph/scip-python/blob/51dd05f5c9d43d87471c2b3bf9f74a9256b627c7/packages/pyright-scip/src/MainCommand.ts)
- `sourcegraph/scip-typescript` @ `7f51391d`:
  [CommandLineOptions.ts](https://github.com/sourcegraph/scip-typescript/blob/7f51391d9770c63be2449463c8eb89f98c9e1468/src/CommandLineOptions.ts)
- `rust-lang/rust-analyzer` @ `37721d14`:
  [cli/flags.rs](https://github.com/rust-lang/rust-analyzer/blob/37721d141370e0904033f7fc3a3abe11c446c4c1/crates/rust-analyzer/src/cli/flags.rs),
  [cli/scip.rs](https://github.com/rust-lang/rust-analyzer/blob/37721d141370e0904033f7fc3a3abe11c446c4c1/crates/rust-analyzer/src/cli/scip.rs),
  [architecture.md](https://github.com/rust-lang/rust-analyzer/blob/37721d141370e0904033f7fc3a3abe11c446c4c1/docs/book/src/contributing/architecture.md)

**Sourcegraph:**

- https://sourcegraph.com/docs/code-navigation/auto-indexing
- https://sourcegraph.com/docs/code-navigation/precise-code-navigation
- https://sourcegraph.com/blog/optimizing-a-code-intelligence-commit-graph-part-2
- `sourcegraph/sourcegraph-public-snapshot` @ `c864f15a`:
  [internal/codeintel/codenav/gittree_translator.go](https://github.com/sourcegraph/sourcegraph-public-snapshot/blob/c864f15af264f0f456a6d8a83290b5c940715349/internal/codeintel/codenav/gittree_translator.go)
- `sourcegraph/zoekt` @ `b55ce727`:
  [index/builder.go](https://github.com/sourcegraph/zoekt/blob/b55ce727df8cfbed76b8a1420445fd0cf93f3256/index/builder.go),
  [gitindex/index.go](https://github.com/sourcegraph/zoekt/blob/b55ce727df8cfbed76b8a1420445fd0cf93f3256/gitindex/index.go)

**Salsa (`salsa-rs/salsa` @ `2d0a5bc6`):**

- [reference/algorithm.md](https://github.com/salsa-rs/salsa/blob/2d0a5bc65d24e0393a3c791cbe4d349d5271d6ab/book/src/reference/algorithm.md)
- [reference/durability.md](https://github.com/salsa-rs/salsa/blob/2d0a5bc65d24e0393a3c791cbe4d349d5271d6ab/book/src/reference/durability.md)
- [plumbing/terminology/backdate.md](https://github.com/salsa-rs/salsa/blob/2d0a5bc65d24e0393a3c791cbe4d349d5271d6ab/book/src/plumbing/terminology/backdate.md)

**tree-sitter (`tree-sitter/tree-sitter` @ `32594ac6`):**

- [docs/src/using-parsers/3-advanced-parsing.md](https://github.com/tree-sitter/tree-sitter/blob/32594ac6e0532a31bcca7fcd043c34ff5e5e007a/docs/src/using-parsers/3-advanced-parsing.md)
- [lib/include/tree_sitter/api.h](https://github.com/tree-sitter/tree-sitter/blob/32594ac6e0532a31bcca7fcd043c34ff5e5e007a/lib/include/tree_sitter/api.h)

**Graphify (`Graphify-Labs/graphify` @ `3671f311`):**

- [graphify/cache.py](https://github.com/Graphify-Labs/graphify/blob/3671f31124fdb1de4ec4e485b1bcd7059726a5f3/graphify/cache.py)
- [graphify/extract.py](https://github.com/Graphify-Labs/graphify/blob/3671f31124fdb1de4ec4e485b1bcd7059726a5f3/graphify/extract.py)
- [graphify/watch.py](https://github.com/Graphify-Labs/graphify/blob/3671f31124fdb1de4ec4e485b1bcd7059726a5f3/graphify/watch.py)

**stack-graphs (`github/stack-graphs` @ `fcb7705d`, archived):**

- [README.md](https://github.com/github/stack-graphs/blob/fcb7705d5b38ae13b3665a9b2c882e5a97243d44/README.md)
- [stack-graphs/src/storage.rs](https://github.com/github/stack-graphs/blob/fcb7705d5b38ae13b3665a9b2c882e5a97243d44/stack-graphs/src/storage.rs)
- [tree-sitter-stack-graphs/src/cli/index.rs](https://github.com/github/stack-graphs/blob/fcb7705d5b38ae13b3665a9b2c882e5a97243d44/tree-sitter-stack-graphs/src/cli/index.rs)
- https://github.blog/open-source/introducing-stack-graphs/

**tantivy (`quickwit-oss/tantivy` @ `0aa896fe`):**

- [ARCHITECTURE.md](https://github.com/quickwit-oss/tantivy/blob/0aa896fe29417107ad0bbab502c1a9fd1077f272/ARCHITECTURE.md)

**Bazel, SQLite, Git:**

- https://bazel.build/remote/caching
- https://bazel.build/query/language
- https://www.sqlite.org/limits.html
- https://git-scm.com/book/en/v2/Git-Internals-Git-Objects
