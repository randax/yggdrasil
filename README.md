# yggdrasil

*The world-tree connecting all the realms of your code.*

Yggdrasil is a self-hosted **Index Server** that continuously syncs git repositories from **GitHub, GitLab, and Codeberg** and serves a **Knowledge Graph** of them — code structure with compiler-precise call/reference edges, cross-repo Dependencies, git history, people and ownership, docs, and Forge Artifacts — to **AI agents over MCP** and **humans over a CLI**, through one curated set of Verbs. A shipped Skill teaches agents how to navigate; a `map` Verb gives them fresh per-repo orientation.

**Status: design phase.** The design is documented and open for comment:

| Document | What it answers |
|---|---|
| [PRD](docs/PRD.md) | What we're building, for whom, and what v1 must do |
| [Architecture (ARD)](docs/ARCHITECTURE.md) | Components, data flow, deployment topologies, NFRs |
| [RFC 0001](docs/rfc/0001-yggdrasil-v1.md) | The full v1 technical design — comments welcome |
| [ADRs](docs/adr/) | Why the load-bearing decisions went the way they did |
| [CONTEXT.md](CONTEXT.md) | The project's vocabulary |

Built in Rust. Postgres + S3-compatible object storage. AGPL-3.0.

## Developing

`docker compose up -d` brings up dev Postgres + MinIO; see
[docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) for ports, credentials, and the
checks CI runs.
