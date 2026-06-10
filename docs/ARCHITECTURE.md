# Yggdrasil — Architecture Document (ARD)

Status: Draft for review · Date: 2026-06-10
Companion docs: [PRD](./PRD.md) · [RFC 0001](./rfc/0001-yggdrasil-v1.md) · [ADRs](./adr/) · [Glossary](../CONTEXT.md)

## Overview

One Rust binary, three roles. Stateless compute around two stateful, self-hostable systems: Postgres (control plane) and S3-compatible object storage (Shards).

```
            Forges (GitHub / GitLab / Codeberg)
                 │ poll + webhooks          │ git fetch / API
                 ▼                          ▼
          ┌─────────────┐  jobs   ┌──────────────────┐
          │ Sync service │───────▶│ Indexing Workers  │  (sandboxed SCIP builds,
          │ + discovery  │        │  (stateless, ×N)  │   tree-sitter, extractors)
          └──────┬──────┘         └─────────┬────────┘
                 │                          │ write Shard, swap pointer
                 ▼                          ▼
       ┌──────────────────┐      ┌───────────────────────┐
       │ Postgres          │      │ Object storage (S3/    │
       │ control plane     │      │ MinIO/Garage):         │
       │ repos·jobs·tokens │      │ immutable per-repo     │
       │ cross-repo edges  │      │ Shards (graph+FTS+ANN) │
       └────────┬─────────┘      └──────────┬────────────┘
                │ bounded fan-out            │ mmap via NVMe/mem cache
                ▼                            ▼
          ┌─────────────────────────────────────┐
          │ Query Nodes (stateless, ×N)          │
          │ Verb engine · REST · MCP (HTTP)      │
          └────────────────┬────────────────────┘
                           │ bearer tokens
            ┌──────────────┼──────────────┐
            ▼              ▼              ▼
        yg CLI      MCP clients      yg mcp (stdio→HTTP proxy)
                    + the Skill
```

## Components

### Control plane (Postgres)
System of record: Forge connections, repo registry + discovery state, sync cursors, job queue (`SELECT … FOR UPDATE SKIP LOCKED`), Shard pointers, Members + bearer tokens, the cross-repo edge index (package → repo, repo → repo dependency edges, global symbol prefixes), and Contributor identity merges. Everything else is rebuildable; Postgres and object storage are the only backup targets.

### Sync service
Per-Forge adapters behind one trait: discovery (orgs/groups → repos, include/exclude rules, private = opt-in), poll loop with conditional requests against rate-limit budgets, optional webhook receiver as accelerator (poll always reconciles, so missed webhooks only cost latency, never correctness). Emits jobs: `fetch`, `index_syntactic`, `index_precise`, `extract_{history,docs,owners,artifacts}`, `embed`.

### Indexing Workers (stateless, horizontal)
Pull jobs from the queue. Two-pass indexing:

1. **Syntactic pass** (fast, always succeeds): tree-sitter parse, heuristic edges, docs/history/owners extraction → publish Shard within minutes of a change.
2. **Precise pass** (slow, best-effort): SCIP indexer runs the repo's build inside a **sandbox** (container, controlled egress for dependency fetch, no credentials beyond read-only clone token); on success its edges overlay the syntactic ones in a new Shard revision.

Every edge records Provenance (`precise` | `syntactic`) + confidence (ADR 0002). A failed build degrades a repo to syntactic — never to absent.

### Shard store (object storage)
One immutable Shard per repo per revision: graph segment (SQLite file as artifact format), full-text segment (tantivy), vector segment (ANN index, M3), plus a manifest. Content-addressed keys; re-index publishes a new Shard and atomically swaps the pointer in Postgres; old Shards garbage-collected after a grace window. No locks, no partial states, trivially cacheable (ADR 0005).

### Query Nodes (stateless, horizontal)
Serve the Verb engine over REST + MCP (Streamable HTTP). Hot Shards are cached memory → NVMe → object storage (turbopuffer/Zoekt pattern). Single-repo Verbs touch one Shard. Cross-repo Verbs (`dependents`, `impact`, org-wide `callers`) consult the control-plane edge index first, then fan out only to the bounded set of implicated Shards. Org-wide `search` fans out across cached tantivy segments with early termination.

### Clients
- **`yg` CLI** — human-ergonomic subcommand per Verb (`yg callers pkg.Symbol --json`), plus `yg admin` (Forge connections, rules, status) and `yg mcp` (stdio→HTTP proxy for MCP clients that can't reach remote servers).
- **MCP** — one tool per Verb, schema-identical to CLI/REST (ADR 0003).
- **The Skill** — versioned with the Verb set; teaches: `map` first, search → traverse → read, trust `precise`, verify `syntactic`.

## Deployment topologies

| Size | Topology |
|---|---|
| Dev / evaluation | `yg serve --role=all` + Postgres + MinIO (docker compose, 3 containers) |
| Standard org | 1–2 API/query nodes, 4–8 workers, managed Postgres, any S3 |
| Large org (design point) | N query nodes behind LB, 20–40+ autoscaled workers, Postgres + read replica, object storage with NVMe cache volumes on query nodes |

Worker fleet math (large org): 5000 repos × ~4 worker-minutes (precise pass, cached deps) ≈ 333 worker-hours → ~40 workers re-index the org overnight; steady-state incremental load is far smaller (only changed repos re-index).

## Non-functional requirements

Targets as in the PRD: verb p95 < 300 ms warm, search p95 < 500 ms warm, syntactic freshness p50 ≤ 8 min (poll) / ≤ 3 min (webhook), 99% SLO attainment, overnight full re-index, all derived state rebuildable from git + Forge APIs.

## Security model

- **Authn:** Admin-issued bearer tokens per Member/agent; OIDC post-v1. TLS terminated at or before query nodes.
- **Authz:** org-visible — every Member sees everything indexed; exposure is controlled by what the Admin indexes (ADR 0001).
- **Build sandboxing:** the precise pass executes repo-controlled code. Containers with read-only source mounts, scoped egress (package registries only), no ambient credentials, resource limits. This is the most security-sensitive surface in the system.
- **Data egress:** embeddings and LLM Concept extraction are pluggable providers, local-capable, **off by default**; no source leaves the deployment without explicit Admin opt-in.

## Failure modes

| Failure | Behavior |
|---|---|
| Missed webhook | Poll loop reconciles within one interval — latency cost only |
| Worker crash mid-index | Job lease expires, retried; immutable Shards mean no partial state visible |
| Precise build fails | Repo stays at syntactic provenance; surfaced in `yg admin status` |
| Query node loss | Stateless — LB routes around; cache rewarms from object storage |
| Postgres loss | Restore from backup; Shard pointers re-validated against object storage manifests |
| Object storage loss | Full re-index from Forges (overnight); control plane intact |

## Why not …

See ADRs: org-visible access (0001), precise-with-fallback extraction (0002), Verbs over a query language (0003), embedded-SQLite-on-one-box (0004, superseded), object-storage-native Shards (0005).
