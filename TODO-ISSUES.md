# TODO — open-issue implementation tracker

Derived from the 2026-07-15 backlog triage. Each item is checked off when its
PR is approved (Qodo + Greptile), merged, and the issue closed.
Critical path: **#59 → {#60, #62, #64} → {#63, #65, #66}**.

## Wave 1 — unblocked keystones (parallelizable)

- [x] **#59** Move search orchestration (fan-out, merge, cursors, snippet hydration) from `yg-api/src/search.rs` into `yg-verbs`; bounded concurrent fan-out; reuse FTS handle. *Critical path — blocks #60/#61/#62/#63/#64.*
- [x] **#40** Torn-Shard repair: manifest-first deletion, GC soft-delete (`reclaiming` state). *Riskiest storage change — blocks #29.*
- [x] **#54** Declarative language packs replacing the seven hand-rolled parser setups; split `yg-index/lib.rs` into pass/resolve/history/gc/worker modules. *Blocks #43.*
- [x] **#46** Graceful shutdown: `with_graceful_shutdown` + SIGTERM drain for server and sync worker.
- [x] **#47** Prometheus `/metrics` endpoint (queue depth, poll lag, Verb latency, cache stats). *Blocks #68.*
- [x] **#55** (remainder) Count-by-visibility summary in `AdminStatusResponse` + CLI. *Per-repo field already shipped.*
- [x] **#42** (remainder) Pin `LC_ALL` for git subprocesses; cap `list_org_repos` pagination; honor `Retry-After`; charge discovery to the Forge budget. *Git deadlines already shipped; pairs with #48.*

## Wave 2 — on the seams

### Forge/sync seam (#53, merged) — in order

- [x] **#79** Typed URL newtypes on control-plane boundary structs (do first — touches structs the rest modify).
- [x] **#78** Resolve repo-URL forge classification against configured forge records.
- [x] **#41** Per-repo discovery reconciliation: survive qualifier conflicts, validate discovered slugs, unify lock order.
- [x] **#48** Control-plane-shared Forge rate budgeting (absorbs #42's discovery-budget remainder).

### Verb-engine seam (#59)

- [x] **#60** Shared `Serialize + Deserialize` wire DTOs; typed CLI parsing (kill `unwrap_or("?")` fallbacks).
- [x] **#61** Fuzzy node ids with ranked candidates (no-such-symbol vs ambiguous).
- [x] **#62** MCP tools call the Verb engine directly (remove 50MB re-buffer and flattened error strings).

### One shard schema bump (batch: one `SCHEMA_VERSION` bump, one reindex)

- [x] **#64** Code tokenizer on file bodies, indexed paths, normalized cross-repo ranking, edge integrity constraints.
- [x] **#43** Language-scoped syntactic resolution; per-file degradation instead of job abort (after #54).
- [x] **#22** Transitive in-repo interface-embed resolution for syntactic IMPLEMENTS.

### Independents

- [x] **#45** Shard cache: LRU eviction, single-flight cold fetches, streaming verification.
- [x] **#44** Cap hub-node fan-out in graph traversals; truncation markers (no schema bump).
- [x] **#58** API protection: per-token rate limits, token expiry, MCP batch caps, server/client timeouts.

## Wave 3 — dependents

- [x] **#63** Signed, re-validated cursors (after #59/#60 consolidate cursor infra).
- [x] **#65** MCP conformance + Skill contract-version handshake (after #62).
- [ ] **#66** CLI UX: machine-readable writes, exit-code classes, flag vocabulary (after #60).
- [ ] **#51** Deployment vehicle: container image + compose profile.
- [ ] ~~**#15** Release packaging (atop #51).~~ *Skipped per owner instruction (2026-07-16).*
- [ ] **#68** Tokens-per-task metrics + orientation-efficiency eval (after #47; baseline after #64/#44).
- [x] **#29** Orphaned object-storage segment reconciler (after #40).

## Wave 4 — closers

- [ ] **#24** Document cold-cache search latency long-tail (after #45/#64 change the numbers).
- [ ] **#56** Docs truth restoration — deliberately last, so docs describe the final state.
- [ ] **#28** ETag conditional requests + rate-limit headers for GitHub polling (unblocked by #53, kept M1+).
- [ ] **#37** Close umbrella PRD when children are done.
