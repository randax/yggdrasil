# Yggdrasil — Product Requirements Document

Status: Draft for review · Date: 2026-06-10 · Amended 2026-11-22 (ADR 0007) · Owner: Øyvind Randa

## Problem

AI coding agents navigate codebases the way 1970s tools do: grep, file listings, and whatever fits in a context window. That breaks down completely at organization scale — an agent working in one repo cannot answer *"who calls this API across our 5000 repos?"*, *"what breaks if I change this?"*, or *"who actually knows this code?"*. Humans have the same problem with worse patience.

Existing tools each solve a fragment ([landscape research, 2026-11](research/team-brain-landscape.md)):

- **Graphify** builds a syntactic (tree-sitter) knowledge graph of a checkout on a developer's machine, shared by committing a JSON file. Its hosted Graphify Cloud (2026) adds push-driven sync, cross-repo graphs and agent memory, but stays syntactic, closed and SaaS-first (self-hosting is Enterprise early access): edges that can't be trusted for impact analysis.
- **Sourcegraph** solved org-scale search and precise code intelligence, but as a heavyweight proprietary platform (closed since 2024, from $16K/year) with no write path and no non-code sources — not an open, lightweight, agent-first graph.
- **Answer engines** (Glean, Unblocked, Atlassian Rovo, Onyx) embed what a team writes and answer with citations, but index code, if at all, as text chunks; none documents a call graph, so they cannot enumerate call sites or say what a change breaks.
- **Memory engines** (Zep/Graphiti, mem0) and GitHub Copilot Memory let agents remember facts, but the facts float free of the code's structure. Copilot Memory re-checks code citations and expires memories after 90 days by default; it is GitHub-only SaaS with no graph.
- **turbopuffer** proves the storage blueprint (object-storage-native indexes, cache tiers) but is a search primitive, not repo-aware.

Nothing self-hostable continuously syncs an organization's repos from their forges, serves a **precise, queryable knowledge graph** designed for AI agents, and lets the team's agents share what they learn about that code — anchored to the code it describes.

## Product

Yggdrasil is a self-hosted **Index Server** that syncs git repositories from **Forges** (GitHub, GitLab, Codeberg), builds a **Knowledge Graph** of code, history, people, docs, and forge artifacts, and serves it to AI agents over **MCP** and to humans over a **CLI** — through one curated set of **Verbs**. A shipped **Skill** teaches agents the navigation method; a **Map** verb gives them fresh per-repo orientation.

**Positioning: the team brain for code** (ADR 0007). The graph is the spine, and team knowledge enters only by attaching to it: Forge Artifacts, Doc Sections and ownership as derived layers, and **Memories** — what Members (human or agent) learned that the sources don't say — as anchored, attributed, lowest-trust claims that go stale when the code they describe changes. Yggdrasil does not become a company-wide answer engine; it is the code back end that agents call over MCP, as can answer engines that accept custom MCP sources (Unblocked does today).

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
- G9. One Verb set as the entire public contract — the graph Verbs search, map, node, neighbors, path, callers, callees, deps, dependents, owners, history, impact, plus the Memory Verbs remember, recall, promote, retract (G15) — identical across CLI, MCP, HTTP. No query language (ADR 0003).
- G10. Full-text search core (code, symbols, docs, commits, artifacts); semantic search via pluggable embedding provider (off by default; when enabled the local provider is the default, external APIs explicit opt-in), shipping in M3. Hybrid lexical+semantic is the target end state: Cursor's published evals ([semsearch](https://cursor.com/blog/semsearch)) show grep+semantic beats either alone, with gains concentrating in large codebases — exactly our design point. Scheduled after the core graph proves out, by explicit decision.

### Interfaces
- G11. MCP over Streamable HTTP natively; `yg` CLI doubles as stdio→HTTP MCP proxy. Bearer-token auth v1.
- G12. The Skill ships with the product, versioned with the Verb set; instructs agents to call `map` first.

### Operations
- G13. Designed and certified for **5000+ repos, 500+ Members**; object-storage-native Shards + Postgres control plane + stateless workers and query nodes (ADR 0005).
- G14. Self-hosting floor: one binary (role modes) + Postgres + S3-compatible store (RustFS/Garage), via docker compose.

### Memory (ADR 0007; numbered after the original goals)
- G15. Members (human or agent) `remember` a fact, decision, convention or gotcha anchored to graph nodes; `recall` it by anchor, scope or full-text query; `promote` it from the author's private overlay into the shared pool at repo or org scope; and affirm or supersede it (via `remember`) or `retract` it. Every Memory carries `asserted` trust, its author and its staleness. Derived Verbs never return Memories, and `recall` returns bodies as untrusted data — claims to verify, never instructions.
- G16. Staleness follows the code: when a published Shard revision changes an anchored node's fingerprint or removes it, its Memories are flagged stale with a reason, never silently deleted. Semantic Memory never expires by time; Episodic Memory is session-bound and expires.
- G17. Safe sharing:
  - Memories are private on write. Promotion, and affirming a stale shared Memory, are explicit, audited and limited to Members granted the right (by default human Members). By default an agent's promotion becomes a proposal to a named reviewer; an Admin may explicitly grant an agent the right.
  - Memories are recallable only while every repo they anchor to is indexed.
  - Writes a secret scanner flags are rejected; per-Member quotas apply.
  - Retraction suppresses a Memory everywhere, and Admin erasure deletes its body. Both go to an append-only object-storage log (object-locked where the store supports it) that is excluded from restores and replayed before reads resume.
  - Memories live in the control plane and are covered by its backups. They are the only graph knowledge that cannot be rebuilt from git and the Forges.

## Non-goals (v1)

- Mirroring forge ACLs / per-user visibility — org-visible by design (ADR 0001).
- A public graph query language (ADR 0003).
- Indexing non-default branches or CR heads.
- Code review, hosting, or CI features — Forges keep their jobs.
- A company-wide answer engine (ADR 0007): no ingestion of chat, ticket systems outside the Forges, or documents outside indexed repos; no synthesized natural-language answers or chat/wiki front end. Agents compose answers from Verb results; answer engines that accept custom MCP sources can call Yggdrasil for the code questions they can't answer.
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

A **cold search** is the first query against a freshly published Shard with an empty local Shard cache. It includes the object-storage round-trip plus FTS segment download and unpack, so its latency scales with Shard size and object-store latency. Observed by a documented run of `cold_search_latency_is_documented` against the dev-compose MinIO with a tiny fixture Shard: cold ≈ 22 ms first search, ≈ 11 ms immediately warm — the cold premium here covers the manifest and FTS-archive fetches from the loopback object store plus a small unpack; production Shards pay the same shape scaled by segment size and store latency. The smoke test emits both figures (visible with --nocapture) and applies only a generous 30 s sanity bound to the cold path; it documents the long tail rather than making it a performance gate.

## Success metrics

- Agent adoption: graph Verb calls per agent session in dogfooding orgs (target: agents choose graph over grep ≥ 70% of navigation tasks).
- Precise coverage: % of org LOC under SCIP-precise indexing (target ≥ 60% for Go/TS/Python/Rust-dominant orgs).
- Freshness SLO attainment ≥ 99%.
- Time-to-orientation: agent answers "where is X handled and who owns it" in an unfamiliar repo in ≤ 3 Verb calls.
- Compounding memory: on a repeated-task scenario added to the docs/EVAL.md harness, an agent given a teammate's promoted Memories uses ≥ 20% fewer Verb calls and tokens than one without; 100% of Memories whose anchor changed are flagged stale before they are recalled.

## Milestones

- **M0 — Tracer (GitHub, syntactic).** One vertical slice: control plane → sync → worker → Shard on object storage → query node → search/node/neighbors/history Verbs → CLI + MCP + Skill v0. Proves the architecture end-to-end.
- **M1 — Precise core.** SCIP workers (Go, TypeScript/JS, Python, Rust), provenance overlay, stable cross-pass Symbol IDs and per-node content fingerprints (prerequisite for ADR 0007), callers/callees/impact, `map` Verb, GitLab support.
- **M2 — Shared memory and the whole graph.** Opens with the Memory slice (ADR 0007): Admin-issued Member tokens if not already landed, then `remember`/`recall`/`promote`/`retract`, Anchors, code-driven staleness, Promotion. Then history/people layers, CODEOWNERS, Doc Sections, cross-repo dependency resolution, Codeberg, Forge Artifacts.
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
- **Memory leakage, poisoning and injection** — a promoted Memory is org-visible (ADR 0001), so a careless claim, a wrong claim or an instruction aimed at other agents spreads. Mitigated by:
  - private-on-write, with Promotion restricted and audited;
  - secret scanning and quotas;
  - `asserted` as the lowest trust level, with bodies returned as untrusted data;
  - code-driven staleness;
  - admission-bound recall;
  - retraction and Admin erasure.
- **Agents acting under human credentials** — an agent that reaches a human's token can bypass the promotion gate. Mitigated by agent-only tokens for `yg mcp`, Skill guidance and an interactive step-up credential for human publishing acts (RFC 0001 §5.1). The server cannot fully verify who is at the keyboard, so the residual risk is stated, not hidden.
- **Competitive window** — Graphify Cloud and Copilot Memory iterate quickly on adjacent ground. Mitigated by sequencing: precise core (M1), then anchored Memory (start of M2), with semantic search — table stakes elsewhere, not separation — left at M3 per G10.

## License & development

AGPL-3.0, developed in the open; DCO/CLA retained for a future dual-licensing option. Self-hosters are the first-class audience — the Codeberg of code intelligence, not the next SaaS.
