# RFC 0001 — Yggdrasil v1 Technical Design

Status: Proposed · Date: 2026-06-10 · Author: Øyvind Randa
Scope: full v1 design (milestones M0–M3). Requirements live in the [PRD](../PRD.md); component overview in [ARCHITECTURE](../ARCHITECTURE.md); vocabulary in [CONTEXT.md](../../CONTEXT.md). Comments welcome on everything; **Open questions** at the end are genuinely open.

## 1. Summary

A Rust workspace producing one binary, `yg`, that is simultaneously the server (`yg serve --role=api|worker|all`), the Admin/Member CLI, and a stdio MCP proxy (`yg mcp`). Stateless API/query nodes and indexing workers around Postgres (control plane) and S3-compatible object storage (immutable per-repo Shards). The v1 contract targets twelve Verbs over REST + MCP; M0 currently serves four.

## 2. Control plane schema (Postgres, sketch)

```sql
forges(id, kind /* github|gitlab|codeberg */, base_url, token_ref, rate_budget)
repos(id, forge_id, remote_id, slug, visibility, default_branch, indexed bool,
      discovery_state /* discovered|included|excluded */, last_synced_commit, …)
rules(id, forge_id, pattern, action /* include|exclude */, applies_to_private bool)
jobs(id, kind, repo_id, state, lease_until, attempts, run_after, finished_at)
shards(repo_id, revision, manifest_key, commit_sha, provenance_level, published_at)
members(id, name, kind /* human|agent */)
tokens(id, member_id, hash, created_at, last_used_at, revoked_at)
packages(id, ecosystem, name)                       -- npm, crates, go modules…
repo_packages(repo_id, package_id, version_range, relation /* provides|depends */)
xrepo_edges(from_repo, to_repo, via_package_id, kind)
contributors(id, display_name)
identities(contributor_id, kind /* email|forge_account */, value, merge_provenance)
```

Job queue = `SELECT … FOR UPDATE SKIP LOCKED` with leases; priorities favor incremental over backfill. No Redis/Kafka in v1 — Postgres is enough at 5000 repos and removing a dependency beats theoretical throughput.

## 3. Sync protocol

- **Discovery loop** (per Forge connection, ~hourly + on demand): list orgs/groups → upsert `repos` with `discovery_state`. Public/internal → `included` unless an exclude rule matches; private → `discovered` until an include rule with `applies_to_private` covers it (ADR 0001). Rule precedence is longest matching glob first, then newest rule for equal-length matches.
- **Poll loop** (default 5 min/repo, jittered, conditional requests): compare branch head via cheap API call; on change enqueue `fetch`. Rate budgeting per Forge token; backoff on 429/abuse signals.
- **Webhook receiver** (optional accelerator): push/CR/issue events → enqueue the same jobs. Secrets per Forge; idempotent with poll (poll is truth, webhooks are latency).
- **Fetch job**: bare clone/fetch into worker-local cache (full history default; `--depth` per-repo override). Forge Artifacts synced via API cursors (updated-since), normalized to common shapes (Change Request, Issue, Review).

## 4. Indexing pipeline

Two-pass per change (ADR 0002):

**Pass 1 — syntactic (target: minutes).** tree-sitter parse of changed files (full parse on first index): Symbols, heuristic call/import/extends edges with confidence; markdown → Doc Sections + `MENTIONS` edges (lexical match against symbol names, tagged); CODEOWNERS → `OWNS` edges; `git log` delta → commits, `TOUCHES`/`AUTHORED` edges; manifest parse → `packages`/`repo_packages`/`xrepo_edges`. Publishes a Shard immediately.

**Pass 2 — precise (target: same hour).** Language detection → SCIP indexer matrix (M1: scip-go, scip-typescript, scip-python, rust-analyzer/scip; M2+: scip-java family, scip-clang, scip-ruby, scip-dotnet). Runs in a sandbox: container, read-only source, egress allowlist (package registries), no credentials, CPU/mem/time limits, per-language dependency caches (e.g. shared module/crate caches) to hit the ~4-min average. Output SCIP → occurrences + precise edges → new Shard revision overlaying provenance. Build failure ⇒ stay syntactic, surface in admin status.

**Cross-repo linking.** SCIP symbol monikers (ecosystem + package + version + descriptor) are the global join key. Workers emit `provides`/`depends` package facts; the control plane resolves them to `xrepo_edges`, which bound all cross-repo query fan-out.

**Identity merge.** Same email or .mailmap entry ⇒ merge (provenance `extracted`); same forge account across Forges via profile URLs ⇒ merge (`extracted`); name+similar-email heuristics ⇒ merge (`inferred`, reversible via `identities.merge_provenance`).

**Embeddings (M3).** `embed` jobs are planned to chunk code along Symbol and Doc Section boundaries — the graph gives semantic chunking for free, no line windows — and produce the vector segment for the next Shard revision. The provider API, local model, and inference runtime are deliberately deferred to M3 benchmarking; external API providers remain opt-in. Hybrid lexical+semantic is the deliberate end state: Cursor's semsearch evals report grep+semantic beating either alone (12.5% mean accuracy gain, growing with codebase size), and the effect concentrates at exactly our scale. Scheduled M3 by explicit decision — the core graph proves out first. LLM Concept extraction (M3, opt-in) is planned to follow the selected provider shape for docs → Concept nodes.

## 5. Graph schema

Node kinds: `Repo, File, Symbol, Package, Commit, Contributor, Team, ChangeRequest, Issue, Review, DocSection, Concept`.

Edge kinds (all carry `provenance ∈ {precise, syntactic, extracted, inferred}` + `confidence ∈ [0,1]` + locations where applicable):

```
DEFINES        File → Symbol            CALLS         Symbol → Symbol
REFERENCES     Symbol/File → Symbol     IMPORTS       File → File/Package
EXTENDS        Symbol → Symbol          IMPLEMENTS    Symbol → Symbol
DEPENDS_ON     Repo → Package/Repo      PROVIDES      Repo → Package
TOUCHES        Commit → File            AUTHORED      Contributor → Commit
OWNS           Contributor/Team → File/Dir
MENTIONS       DocSection/Issue/ChangeRequest → Symbol/File
PART_OF        ChangeRequest → Commit   CLOSES        ChangeRequest → Issue
```

Node IDs are stable content-derived strings (`repo:github.com/acme/api`, `sym:<scip-moniker>`, `doc:<repo>:<path>#<heading-slug>`), so Shard rebuilds don't invalidate references held by agents mid-session.

## 6. Shard format

```
shards/<repo-id>/<revision>/
  manifest.json      # commit, pass level, segment checksums, schema version
  graph.sqlite       # nodes, edges, occurrences (SQLite as *artifact format*)
  fts/               # tantivy segment directory
  vec.*              # ANN segment (M3; library/format open — §11 Q2)
```

Immutable; pointer swap in `shards` table; GC after grace window. Query nodes mmap segments from an NVMe cache keyed by checksum. SQLite survives ADR 0004's supersession as the *graph segment format* — single-file, mmap-friendly, queryable in-process — while object storage + Postgres carry the multi-node story (ADR 0005).

## 7. Verb contract (ADR 0003)

The v1 contract targets twelve Verbs. For the implemented rows, the shared
typed request definitions drive REST (`POST /v1/verbs/<name>`), MCP tools
(`<name>`), and CLI subcommands; planned rows describe milestone intent, not
available endpoints. The served set is defined by `yg_verbs::Verb::ALL` and
`VERB_TOOLS`. Sketch:

| Verb | Status | In | Out |
|---|---|---|---|
| `search` | implemented | query, kinds?, repos?, mode: lexical (semantic/hybrid planned-M3) | ranked node refs + snippets |
| `map` | planned-M1 | repo | entry points, landmark Symbols (centrality), doc index, owners, stats |
| `node` | implemented | id | full node + edge summary |
| `neighbors` | implemented | id, edge_kinds?, direction?, depth≤3 | subgraph |
| `path` | planned-M2 | from, to, max_len | shortest path(s) with edge provenance |
| `callers` | planned-M1 | symbol id, transitive? | call sites with locations |
| `callees` | planned-M1 | symbol id, transitive? | call sites with locations |
| `deps` | planned-M2 | repo\|package\|symbol | dependency closure (xrepo-bounded) |
| `dependents` | planned-M2 | repo\|package\|symbol | dependency closure (xrepo-bounded) |
| `owners` | planned-M2 | file\|dir\|symbol | Contributors/Teams + ownership basis |
| `history` | implemented | file or symbol, since? | commits touching the file or symbol's defining file, Contributors |
| `impact` | planned-M1 | symbol\|file, depth | affected Symbols→Files→Repos, grouped, provenance-annotated |

Lexical search ranks with BM25 inside each repository, then divides every
score by that repository's maximum score before the cross-repository merge.
This maps each repository's best hit to `1.0`, preserves its internal score
ratios, and avoids comparing corpus-dependent raw BM25 scales. The final merge
is deterministic: normalized score descending, then repository and node id
ascending.

Verb additions are minor-version events; removals are major. The Skill is versioned against this table.

## 8. API, auth, clients

- REST + MCP Streamable HTTP on the same port; bearer tokens (`Authorization: Bearer ygt_…`), per-Member, hashed at rest, revocable. OIDC post-v1.
- Operational surface (non-Verb): `GET /healthz` — unauthenticated readiness, a bare `ok`/`error` per dependency (control plane, object storage; failure detail is logged server-side, never returned to anonymous callers); `GET /v1/status` — version and indexed-repo count in the body, uptime in the `x-yggdrasil-uptime-seconds` header (bodies are cache-stable; volatile values ride in headers), behind auth (any valid token, including Member), surfaced as `yg status [--json]`. Every route except `/healthz` requires a token, including unmatched paths. Default listen address `127.0.0.1:7311`. Until Admin-issued Member tokens land (`members`/`tokens`, §2), the server boots from a bootstrap Admin token in `YG_BOOTSTRAP_TOKEN` — held in memory, never stored; the `ygt_` prefix is convention, not enforced. The richer `yg admin status` (§4) arrives with the admin surface.
- `yg mcp` proxies stdio↔HTTP for local-only MCP clients, reading `YG_SERVER`/`YG_TOKEN` from config (`~/.config/yg/config.toml`).
- CLI: `yg <verb> …` for Members; `yg admin forge add|rules|status`, `yg admin token issue|revoke` for Admins; `--json` everywhere; human output is the default.
- The Skill: markdown shipped in-repo + `yg skill install` (drops into agent skill dirs). Its M0 content covers when to use the graph vs reading files; the division of truth (yggdrasil answers org-truth at the default branch; the agent's local tools own working-tree truth, including uncommitted edits); `search`-first orientation; provenance trust rules; the four implemented Verbs; and failure etiquette (graph thin ⇒ fall back gracefully). It moves to `map`-first orientation when that planned-M1 Verb exists.

## 9. Crate layout

```
crates/
  yg-cli        # binary: subcommands, serve roles, mcp proxy
  yg-api        # REST + hand-rolled MCP server (axum)
  yg-verbs      # verb engine (pure: control-plane + shard reads)
  yg-shard      # shard read/write, cache tier, formats
  yg-control    # postgres models, job queue
  yg-sync       # forge trait + GitHub/generic-git adapters, polling and sync workers
  yg-index      # syntactic indexing, history extraction, Shard publication and reconciliation
```

Key dependencies present today: axum, sqlx, rusqlite (read path), tantivy,
tree-sitter grammars, and object_store. The workspace uses the system `git`
executable for repository operations and implements its MCP transport directly
over axum and serde_json. There is no MCP framework, Rust git implementation,
inference runtime, or ANN library in the current lockfile.

**Decision — implement the M0 MCP surface directly.** The earlier `rmcp` choice
was dropped. The served MCP subset is a small JSON-RPC/Streamable HTTP surface
whose tool inventory and schemas come from `yg-verbs`, so a direct axum +
serde_json implementation keeps the transport tied to the same typed Verb
catalog without another framework dependency. Cost: yggdrasil owns protocol
conformance, negotiation, framing, and error behavior; MCP conformance tests are
therefore part of the contract rather than delegated to a library.

**Decision — shell out to the system git CLI.** The earlier `gitoxide` choice
was dropped rather than retained as a primary implementation with an escape
hatch. Workers invoke non-interactive, locale-pinned git commands with explicit
deadlines. This reuses Git's mature protocol and repository behavior and keeps
one implementation path. Cost: the deployment image must contain git, process
execution is heavier than in-process calls, and command environment, output,
timeouts, and cleanup are yggdrasil's responsibility.

**Decision — defer inference and ANN dependencies to M3 evidence.** The earlier
`ort`/local-ONNX direction and leading ANN candidate are not current dependency
choices. M0 carries no provider implementation, and M3 will select the provider
API, local model, runtime, and vector index from benchmarks. Cost: semantic
search and Concept extraction remain unavailable until that decision and its
implementation land; benefit: the workspace does not freeze a runtime or
artifact format before the graph and retrieval workload can measure it.

## 10. Alternatives considered

Recorded as ADRs: org-visible vs ACL mirroring (0001); precise+fallback vs tree-sitter-only vs precise-only (0002); Verbs vs Cypher/GQL (0003); embedded single-box vs object-storage-native (0004→0005); plus in-question rejections (Kuzu post-shutdown, everything-in-Postgres, specialist cluster zoo, Go and Rust+Go hybrid, webhook-only sync, generated per-repo skills).

## 11. Open questions

1. SCIP language priority beyond M1's four — Java family next, or C/C++ (hardest, big payoff)?
2. ANN library: usearch vs hnsw_rs vs lance — decide at M3 with benchmarks.
3. Build caching strategy for the precise pass (shared registry caches vs sccache-style vs Nix) — biggest lever on worker fleet size.
4. Monorepo sub-projects: one Shard per repo or per workspace member? (Affects `map` quality in giant repos.)
5. Org-wide search tiering, lexical *and* vector: pure fan-out with early termination vs a small global "top symbols" index in the control plane; vector fan-out across thousands of per-repo ANN segments may want repo-scoping hints or a coarse global tier.
6. CR-head indexing (the rejected compute bonfire) — revisit as opt-in per-repo flag post-v1?
7. Webhook ingress hardening for internet-exposed deployments (signature schemes differ per Forge).
8. Local default embedding model selection: benchmark open code-retrieval models (and chunking-context variants) at M3, together with the ANN-library decision.

### Closed: Shard schema evolution

A `SCHEMA_VERSION` bump changes the deterministic Shard revision suffix. The
read path refuses older-schema artifacts, and each indexing worker checks at
boot for repositories whose current Shard lacks the new suffix, queues one
index job per repository (without duplicating work already in flight), and
publishes a current-schema Shard before swapping the pointer. Schema evolution
therefore means re-indexing the fleet, not migrating a Shard on read.

The cost is intentionally explicit: every schema bump requires one full
syntactic rebuild and new Shard publication for each included repository that
already has a synced commit and current Shard, consuming fleet worker time,
Git-cache and object-store I/O, and convergence time. The current implementation
does not establish a measured duration for that operation. For scale context,
the PRD's future full-org budget is approximately 5000 repositories × 4
worker-minutes, or about 333 worker-hours with the precise pass and cached
dependencies; roughly 40 workers make that planning estimate an overnight
operation. If the fleet-wide rebuild cost becomes unacceptable at scale, lazy
migration on read is the escape hatch to evaluate then, not behavior the current
system claims to provide.

## 12. Rollout

M0 ships the tracer (GitHub + syntactic + the search/node/neighbors/history Verbs + CLI/MCP/Skill v0) and is the architecture's proof; M1 adds the precise pass + callers/callees/impact + `map` + GitLab; M2 completes the graph layers + Codeberg; M3 adds semantic + webhook accelerators + the 5000-repo certification run (synthetic org, published numbers). Each milestone ends with a dogfooding gate: agents must prefer the graph over grep for the navigation tasks it claims.
