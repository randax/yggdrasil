# RFC 0001 — Yggdrasil v1 Technical Design

Status: Proposed · Date: 2026-06-10 · Author: Øyvind Randa
Scope: full v1 design (milestones M0–M3). Requirements live in the [PRD](../PRD.md); component overview in [ARCHITECTURE](../ARCHITECTURE.md); vocabulary in [CONTEXT.md](../../CONTEXT.md). Comments welcome on everything; **Open questions** at the end are genuinely open.

## 1. Summary

A Rust workspace producing one binary, `yg`, that is simultaneously the server (`yg serve --role=api|worker|all`), the admin/user CLI, and a stdio MCP proxy (`yg mcp`). Stateless API/query nodes and indexing workers around Postgres (control plane) and S3-compatible object storage (immutable per-repo Shards). Public contract: twelve Verbs over REST + MCP.

## 2. Control plane schema (Postgres, sketch)

```sql
forges(id, kind /* github|gitlab|codeberg */, base_url, token_ref, rate_budget)
repos(id, forge_id, remote_id, slug, visibility, default_branch, indexed bool,
      discovery_state /* discovered|included|excluded */, last_synced_commit, …)
rules(id, forge_id, pattern, action /* include|exclude */, applies_to_private bool)
jobs(id, kind, repo_id, payload jsonb, state, lease_until, attempts, priority)
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

- **Discovery loop** (per Forge connection, ~hourly + on demand): list orgs/groups → upsert `repos` with `discovery_state`. Public/internal → `included` unless an exclude rule matches; private → `discovered` until an include rule with `applies_to_private` covers it (ADR 0001).
- **Poll loop** (default 5 min/repo, jittered, conditional requests): compare branch head via cheap API call; on change enqueue `fetch`. Rate budgeting per Forge token; backoff on 429/abuse signals.
- **Webhook receiver** (optional accelerator): push/CR/issue events → enqueue the same jobs. Secrets per Forge; idempotent with poll (poll is truth, webhooks are latency).
- **Fetch job**: bare clone/fetch into worker-local cache (full history default; `--depth` per-repo override). Forge Artifacts synced via API cursors (updated-since), normalized to common shapes (Change Request, Issue, Review).

## 4. Indexing pipeline

Two-pass per change (ADR 0002):

**Pass 1 — syntactic (target: minutes).** tree-sitter parse of changed files (full parse on first index): Symbols, heuristic call/import/extends edges with confidence; markdown → Doc Sections + `MENTIONS` edges (lexical match against symbol names, tagged); CODEOWNERS → `OWNS` edges; `git log` delta → commits, `TOUCHES`/`AUTHORED` edges; manifest parse → `packages`/`repo_packages`/`xrepo_edges`. Publishes a Shard immediately.

**Pass 2 — precise (target: same hour).** Language detection → SCIP indexer matrix (M1: scip-go, scip-typescript, scip-python, rust-analyzer/scip; M2+: scip-java family, scip-clang, scip-ruby, scip-dotnet). Runs in a sandbox: container, read-only source, egress allowlist (package registries), no credentials, CPU/mem/time limits, per-language dependency caches (e.g. shared module/crate caches) to hit the ~4-min average. Output SCIP → occurrences + precise edges → new Shard revision overlaying provenance. Build failure ⇒ stay syntactic, surface in admin status.

**Cross-repo linking.** SCIP symbol monikers (ecosystem + package + version + descriptor) are the global join key. Workers emit `provides`/`depends` package facts; the control plane resolves them to `xrepo_edges`, which bound all cross-repo query fan-out.

**Identity merge.** Same email or .mailmap entry ⇒ merge (provenance `extracted`); same forge account across Forges via profile URLs ⇒ merge (`extracted`); name+similar-email heuristics ⇒ merge (`inferred`, reversible via `identities.merge_provenance`).

**Embeddings (M3).** `embed` jobs chunk code along Symbol and Doc Section boundaries — the graph gives semantic chunking for free, no line windows — then a provider trait (`LocalOnnx` default, a small code-retrieval embedding model via `ort`; external API providers opt-in) produces the vector segment for the next Shard revision. Hybrid lexical+semantic is the deliberate end state: Cursor's semsearch evals report grep+semantic beating either alone (12.5% mean accuracy gain, growing with codebase size), and the effect concentrates at exactly our scale. Scheduled M3 by explicit decision — the core graph proves out first. LLM Concept extraction (M3, opt-in) mirrors the provider shape for docs → Concept nodes.

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
  vec.usearch        # ANN segment (M3)
```

Immutable; pointer swap in `shards` table; GC after grace window. Query nodes mmap segments from an NVMe cache keyed by checksum. SQLite survives ADR 0004's supersession as the *graph segment format* — single-file, mmap-friendly, queryable in-process — while object storage + Postgres carry the multi-node story (ADR 0005).

## 7. Verb contract (ADR 0003)

Twelve Verbs, identical schemas across REST (`POST /v1/verbs/<name>`), MCP tools (`yg_<name>`), and CLI subcommands. Common envelope: pagination cursor, `provenance_min` filter, JSON in/out. Sketch:

| Verb | In | Out |
|---|---|---|
| `search` | query, kinds?, repos?, mode: lexical\|semantic\|hybrid | ranked node refs + snippets |
| `map` | repo | entry points, landmark Symbols (centrality), doc index, owners, stats |
| `node` | id | full node + edge summary |
| `neighbors` | id, edge_kinds?, direction?, depth≤3 | subgraph |
| `path` | from, to, max_len | shortest path(s) with edge provenance |
| `callers` / `callees` | symbol id, transitive? | call sites with locations |
| `deps` / `dependents` | repo\|package\|symbol | dependency closure (xrepo-bounded) |
| `owners` | file\|dir\|symbol | Contributors/Teams + ownership basis |
| `history` | node, since? | commits/CRs touching it, authors |
| `impact` | symbol\|file, depth | affected Symbols→Files→Repos, grouped, provenance-annotated |

Verb additions are minor-version events; removals are major. The Skill is versioned against this table.

## 8. API, auth, clients

- REST + MCP Streamable HTTP on the same port; bearer tokens (`Authorization: Bearer ygt_…`), per-Member, hashed at rest, revocable. OIDC post-v1.
- `yg mcp` proxies stdio↔HTTP for local-only MCP clients, reading `YG_SERVER`/`YG_TOKEN` from config (`~/.config/yg/config.toml`).
- CLI: `yg <verb> …` for Members; `yg admin forge add|rules|status`, `yg admin token issue|revoke` for Admins; `--json` everywhere; human output is the default.
- The Skill: markdown shipped in-repo + `yg skill install` (drops into agent skill dirs). Content: when to use the graph vs reading files; the division of truth (yggdrasil answers org-truth at the default branch; the agent's local tools own working-tree truth, including uncommitted edits); `map`-first orientation; provenance trust rules; verb cookbook; failure etiquette (graph thin ⇒ fall back gracefully).

## 9. Crate layout

```
crates/
  yg-cli        # binary: subcommands, serve roles, mcp proxy
  yg-api        # REST + MCP server (axum + rmcp)
  yg-verbs      # verb engine (pure: control-plane + shard reads)
  yg-shard      # shard read/write, cache tier, formats
  yg-control    # postgres models, job queue
  yg-sync       # forge trait + github/gitlab/forgejo adapters (Codeberg runs Forgejo), webhooks
  yg-index      # tree-sitter pass, scip ingestion, extractors, sandbox driver
  yg-providers  # embedding + llm provider traits, local-onnx impl
```

Key dependencies: axum, sqlx, rusqlite (read path), tantivy, tree-sitter grammars, gitoxide (+ git CLI escape hatch), rmcp, ort, usearch, object_store.

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
8. Shard schema evolution: lazy migration on read vs background re-shard on version bump.
9. Local default embedding model selection: benchmark open code-retrieval models (and chunking-context variants) at M3, together with the ANN-library decision.

## 12. Rollout

M0 ships the tracer (GitHub + syntactic + the search/node/neighbors/history Verbs + CLI/MCP/Skill v0) and is the architecture's proof; M1 adds the precise pass + `map` + GitLab; M2 completes the graph layers + Codeberg; M3 adds semantic + webhook accelerators + the 5000-repo certification run (synthetic org, published numbers). Each milestone ends with a dogfooding gate: agents must prefer the graph over grep for the navigation tasks it claims.
