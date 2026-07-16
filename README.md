# yggdrasil

*The world-tree connecting all the realms of your code.*

Yggdrasil is a self-hosted **Index Server** for serving a repository Knowledge
Graph to **AI agents over MCP** and **humans over a CLI**. The long-term v1
scope is described in the [PRD](docs/PRD.md); the list below separates that
roadmap from behavior that exists today.

## Milestone status

**M0 — Tracer: implemented.** The current vertical slice:

- syncs and polls GitHub repositories, builds a syntactic graph with git
  history, stores immutable Shards in S3-compatible object storage, and serves
  them through Postgres-backed API and worker roles;
- serves exactly four Verbs — `search`, `node`, `neighbors`, and `history` —
  through REST, authenticated MCP tools, and CLI subcommands;
- ships a Claude Code navigation Skill installed by `yg skill install`, stamped
  with the served Verb contract version;
- ships a container image and a single-host Compose `deployment` profile; and
- exposes Prometheus metrics for Verb latency and response size, job queue
  depth, Forge poll lag, and Shard-cache activity.

**M1–M3 — planned.** Compiler-precise indexing, the other eight v1 Verbs,
GitLab and Codeberg support, the remaining graph layers, semantic retrieval,
webhook acceleration, and scale certification are roadmap work, not current
capabilities. See the [PRD milestones](docs/PRD.md#milestones) for their intended
sequence and [TODO-ISSUES.md](TODO-ISSUES.md) for the implementation tracker.

## Design documents

| Document | What it answers |
|---|---|
| [PRD](docs/PRD.md) | What we're building, for whom, and the M0–M3 milestones |
| [Architecture (ARD)](docs/ARCHITECTURE.md) | Components, data flow, deployment topologies, NFRs |
| [RFC 0001](docs/rfc/0001-yggdrasil-v1.md) | The full v1 technical design — comments welcome |
| [ADRs](docs/adr/) | Why the load-bearing decisions went the way they did |
| [CONTEXT.md](CONTEXT.md) | The project's vocabulary |

Built in Rust. Postgres + S3-compatible object storage. AGPL-3.0.

## Developing

`docker compose up -d` brings up dev Postgres + MinIO; see
[docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) for ports, credentials, and the
checks CI runs.
