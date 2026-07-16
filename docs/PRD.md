# Yggdrasil — Product Requirements Document

Status: Draft for review · Date: 2026-06-10 · Owner: Øyvind Randa

## Problem

AI coding agents navigate codebases the way 1970s tools do: grep, file listings, and whatever fits in a context window. That breaks down completely at organization scale — an agent working in one repo cannot answer *"who calls this API across our 5000 repos?"*, *"what breaks if I change this?"*, or *"who actually knows this code?"*. Humans have the same problem with worse patience.

Existing tools each solve a fragment:

- **graphify** builds a knowledge graph of one checkout, once, with syntactic (tree-sitter) accuracy — no server, no sync, no org view, edges that can't be trusted for impact analysis.
- **Sourcegraph** solved org-scale search and precise code intelligence, but as a heavyweight proprietary platform, not an agent-first, self-hostable graph.
- **turbopuffer** proves the storage blueprint (object-storage-native indexes, cache tiers) but is a search primitive, not repo-aware.

Nothing self-hostable continuously syncs an organization's repos from their forges and serves a **precise, queryable knowledge graph** designed for AI agents.

## Product

Yggdrasil is a self-hosted **Index Server** that syncs git repositories from **Forges** (GitHub, GitLab, Codeberg), builds a **Knowledge Graph** of code, history, people, docs, and forge artifacts, and serves it to AI agents over **MCP** and to humans over a **CLI** — through one curated set of **Verbs**. A shipped **Skill** teaches agents the navigation method; a **Map** verb gives them fresh per-repo orientation.

*The world-tree connecting all the realms of your code.*

## Roles

- **Admin** — operates the deployment: connects Forges, sets discovery rules, wires optional providers (embeddings, LLM).
- **Member** — an engineer or their AI agent, querying the graph via CLI/MCP with a bearer token.
- **Contributor** — appears *in* the graph (wrote or owns the code); not necessarily a Member.

## Goals (v1)

### Sync
- G1. Connect GitHub, GitLab, and Codeberg orgs/groups; auto-discover repos; include/exclude rules. Public and internal repos auto-index; **private repos are explicit opt-in** (ADR 0001).
- G2. Poll-based convergence (default 5 min, configurable) plus optional webhook accelerator (RFC 0001 §3).
- G3. Default branch fully indexed; full history by default (shallow configurable). Change Requests appear as Forge Artifacts without head indexing.

### Knowledge Graph
- G4. Code structure with **compiler-precise** call/reference edges via SCIP indexers, tree-sitter fallback where SCIP can't reach; every edge carries Provenance + confidence (ADR 0002).
- G5. Cross-repo Dependency edges resolved through package manifests to indexed repos.
- G6. Git history layer: commits, Contributors (identity-merged across Forges via .mailmap + tagged heuristics).
- G7. Forge Artifacts: Change Requests, issues, reviews, linked to commits and code.
- G8. Doc Sections as nodes with doc→code edges; CODEOWNERS-derived ownership; LLM-extracted Concepts via pluggable provider (**off by default; no source leaves the server without Admin opt-in**).

### Query
- G9. One Verb set as the entire public contract — search, map, node, neighbors, path, callers, callees, deps, dependents, owners, history, impact — identical across CLI, MCP, HTTP. No query language (ADR 0003).
- G10. Full-text search core (code, symbols, docs, commits, artifacts); semantic search via pluggable embedding provider (off by default; when enabled the local provider is the default, external APIs explicit opt-in), shipping in M3. Hybrid lexical+semantic is the target end state: Cursor's published evals ([semsearch](https://cursor.com/blog/semsearch)) show grep+semantic beats either alone, with gains concentrating in large codebases — exactly our design point. Scheduled after the core graph proves out, by explicit decision.

### Interfaces
- G11. MCP over Streamable HTTP natively; `yg` CLI doubles as stdio→HTTP MCP proxy. Bearer-token auth v1.
- G12. The Skill ships with the product, versioned with the Verb set; instructs agents to call `map` first.

### Operations
- G13. Designed and certified for **5000+ repos, 500+ Members**; object-storage-native Shards + Postgres control plane + stateless workers and query nodes (ADR 0005).
- G14. Self-hosting floor: one binary (role modes) + Postgres + S3-compatible store (MinIO/Garage), via docker compose.

## Non-goals (v1)

- Mirroring forge ACLs / per-user visibility — org-visible by design (ADR 0001).
- A public graph query language (ADR 0003).
- Indexing non-default branches or CR heads.
- Code review, hosting, or CI features — Forges keep their jobs.
- IDE plugins, web UI (CLI/MCP first; a read-only web viewer may ride along later).
- SaaS offering — self-hosted only, for now.
- Indexing local working trees / uncommitted edits (Cursor's territory) — yggdrasil serves org-truth from the default branch; the agent's local tools own working-tree truth. The Skill documents this division of labor.

## Non-functional targets

| Dimension | Target |
|---|---|
| Scale | 5000+ repos, 500+ Members, ~100M LOC org-wide |
| Verb latency | p95 < 300 ms (warm cache) |
| Search latency | p95 < 500 ms (warm), cold long-tail characterized below |
| Freshness (poll path) | change → syntactic layers queryable: p50 ≤ 8 min; precise layer when its build lands |
| Freshness (webhook path) | p50 ≤ 3 min to syntactic layers |
| Full-org re-index | overnight with ~40 workers (≈ 5000 repos × ~4 worker-min) |
| Durability | All Shards rebuildable from git + Forge APIs; Postgres is the only stateful backup target besides object storage |
| Security | Sandboxed builds (SCIP runs repo-controlled code), bearer tokens, default-deny external providers |

A **cold search** is the first query against a freshly published Shard with an empty local Shard cache. It includes the object-storage round-trip plus FTS segment download and unpack, so its latency scales with Shard size and object-store latency. Observed 2026-07-17 by `cold_search_latency_is_documented` against the dev-compose MinIO with a tiny fixture Shard: cold ≈ 22 ms first search, ≈ 11 ms immediately warm — the cold premium here covers the manifest and FTS-archive fetches from the loopback object store plus a small unpack; production Shards pay the same shape scaled by segment size and store latency. The smoke test emits both figures (visible with --nocapture) and applies only a generous 30 s sanity bound to the cold path; it documents the long tail rather than making it a performance gate.

## Success metrics

- Agent adoption: graph Verb calls per agent session in dogfooding orgs (target: agents choose graph over grep ≥ 70% of navigation tasks).
- Precise coverage: % of org LOC under SCIP-precise indexing (target ≥ 60% for Go/TS/Python/Rust-dominant orgs).
- Freshness SLO attainment ≥ 99%.
- Time-to-orientation: agent answers "where is X handled and who owns it" in an unfamiliar repo in ≤ 3 Verb calls.

## Milestones

- **M0 — Tracer (GitHub, syntactic).** One vertical slice: control plane → sync → worker → Shard on object storage → query node → search/node/neighbors/history Verbs → CLI + MCP + Skill v0. Proves the architecture end-to-end.
- **M1 — Precise core.** SCIP workers (Go, TypeScript/JS, Python, Rust), provenance overlay, callers/callees/impact, `map` Verb, GitLab support.
- **M2 — The whole graph.** History/people layers, CODEOWNERS, Doc Sections, cross-repo dependency resolution, Codeberg, Forge Artifacts.
- **M3 — Semantic + scale certification.** Embedding provider + vector Shards (opt-in), LLM Concept extraction (opt-in), webhook accelerators, 5000-repo load certification.

## Post-v1 directions

- Per-user visibility via forge ACL mirroring (revisits ADR 0001).
- Org-local feedback-trained retrieval: with opt-in telemetry, learn from this deployment's Verb-call traces which results actually helped agents, and rerank/fine-tune the embedding provider on it — Cursor's training loop, applied per-deployment instead of centrally.
- Opt-in CR-head indexing for graph-diff previews of open Change Requests.

## Risks

- **SCIP indexer maturity varies by language** — mitigated by the syntactic fallback and provenance tags (ADR 0002).
- **Builds are arbitrary code execution** — mitigated by containerized sandboxes with controlled egress; this is the security-critical surface.
- **Scope weight** — mitigated by tracer-bullet staging; M0 is deliberately thin.
- **Identity merge errors** (Contributors across Forges) — merges are tagged and reversible like all inferred data.

## License & development

AGPL-3.0, developed in the open; DCO/CLA retained for a future dual-licensing option. Self-hosters are the first-class audience — the Codeberg of code intelligence, not the next SaaS.
