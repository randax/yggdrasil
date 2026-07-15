# Developing yggdrasil

## Prerequisites

- Rust (stable — `rust-toolchain.toml` pins the channel and pulls in rustfmt + clippy)
- Docker with the compose plugin

## Dev services

`docker compose up -d` starts the two backing services the Index Server needs:

| Service | Image | Endpoint | Credentials |
|---|---|---|---|
| Postgres (control plane) | `postgres:17` | `localhost:5432`, database `yggdrasil` | `yggdrasil` / `yggdrasil` |
| MinIO S3 API (Shard storage) | `minio/minio` (pinned, see below) | `http://localhost:9000` | `yggdrasil` / `yggdrasil` |
| MinIO console | — | `http://localhost:9001` | `yggdrasil` / `yggdrasil` |

Connection string: `postgres://yggdrasil:yggdrasil@localhost:5432/yggdrasil`

MinIO is pinned to `RELEASE.2025-04-22T22-12-26Z`, the last release with the
full community console; the S3 API is what yggdrasil actually needs. A
one-shot `minio-init` service creates the `yggdrasil` Shard bucket, so a
clean stack is immediately usable. (Compose versions older than ~v2.22 fail
`up --wait` when that one-shot exits; use the CI sequence instead:
`docker compose up -d --wait postgres minio && docker compose run --rm minio-init`.)

If a default host port collides with something already running, override it:
`YG_POSTGRES_PORT`, `YG_MINIO_PORT`, `YG_MINIO_CONSOLE_PORT` (e.g.
`YG_POSTGRES_PORT=15432 docker compose up -d`).

These credentials are for local development only. Data persists in named
volumes (`yggdrasil-dev_postgres-data`, `yggdrasil-dev_minio-data`);
`docker compose down -v` resets everything.

## Running the Index Server

The server is configured through `YG_*` environment variables; everything
defaults to the dev compose stack above except the bootstrap Admin token,
which is required. All of them resolve through one typed config
(`crates/yg-cli/src/deploy_config.rs` — the source of truth this table
mirrors), and `yg config-check [--role=api|worker|all]` prints the resolved
configuration (credentials redacted) with every validation error, without
starting the server. Each role resolves, validates, and reports only the
settings its process uses — a worker ignores `YG_LISTEN` and, unless its
optional metrics listener is enabled, `YG_BOOTSTRAP_TOKEN`; an api process ignores the poll/GC cadences and
`YG_GIT_CACHE`. The one exception is the Forge token: its env var
name is stored per Forge org in the control plane (`YG_GITHUB_TOKEN` by
default), so the Sync worker reads it per job and config-check — which
never connects — cannot report it:

| Variable | Default | Purpose |
|---|---|---|
| `YG_BOOTSTRAP_TOKEN` | — (required for api/all and authenticated worker metrics) | Bootstrap Admin bearer token |
| `YG_LISTEN` | `127.0.0.1:7311` | Server bind address |
| `YG_WORKER_METRICS_ADDR` | — (disabled) | Worker-only `/metrics` bind address (for example `0.0.0.0:9400`); absent means no HTTP listener |
| `YG_METRICS_UNAUTHENTICATED` | `false` | Expose `GET /metrics` without a bearer token for scraper convenience |
| `YG_DATABASE_URL` | dev compose Postgres | Control-plane database |
| `YG_S3_ENDPOINT` | `http://localhost:9000` | Object storage endpoint |
| `YG_S3_BUCKET` | `yggdrasil` | Shard bucket |
| `YG_S3_ACCESS_KEY` / `YG_S3_SECRET_KEY` | `yggdrasil` / `yggdrasil` | Object storage credentials |
| `YG_S3_REGION` | `us-east-1` | Object storage region |
| `YG_S3_PREFIX` | *(empty)* | Key prefix all objects land under; empty means the bucket root |
| `YG_SHARD_CACHE` | `./data/shard-cache` | Server-local tier of Shard segments |
| `YG_GIT_CACHE` | `./data/git` | Worker-local cache of bare clones |
| `YG_GITHUB_TOKEN` | — (optional) | Forge token for `github.com` Sync |
| `YG_POLL_INTERVAL` | `300` | Seconds between a repo's default-branch head checks (per-repo override: `repo add --poll-interval`) |
| `YG_DISCOVERY_INTERVAL` | `3600` | Seconds between connected-forge org discovery reconciliations |
| `YG_GC_GRACE` | `3600` | Seconds a superseded Shard is kept before it is garbage-collected |
| `YG_GC_INTERVAL` | `600` | Seconds between Shard garbage-collection sweeps |
| `YG_JOB_RETENTION` | `604800` | Seconds a terminal job row is kept before the GC cadence removes it (bounds queue-table growth) |

An invalid value (an unparseable listen address, a duration that is not a
whole number of seconds) refuses to boot with every problem listed, not
just the first.

`yg serve --role=api|worker|all` picks what the process runs: `api` serves
HTTP only, `worker` drains the Sync and indexing queues (it needs the
control plane, a git cache, and object storage — Shards land there — but no
API listen address), and `all` runs both in one process. A worker binds no HTTP
listener by default. Set `YG_WORKER_METRICS_ADDR` to expose its process-local
`GET /metrics`; the endpoint requires `YG_BOOTSTRAP_TOKEN` unless
`YG_METRICS_UNAUTHENTICATED=true` is set behind a deployment network boundary.

### Forge token scope

Sync only ever reads from a Forge — give it read-only credentials. For
GitHub, use a **fine-grained personal access token** with *Contents:
Read-only* (and *Metadata: Read-only*, which GitHub adds automatically) on
the repositories you sync. Classic tokens can't express read-only access to
private repositories (`repo` grants write), so prefer fine-grained. Public
repositories sync without any token, within GitHub's anonymous rate limits.
The worker passes the token to git per invocation (HTTP header via
process-local config); it is never written to disk or stored in the control
plane.

The CLI talks to a server via `YG_SERVER` (default `http://127.0.0.1:7311`)
and `YG_TOKEN`; both can instead be set as top-level `server` and `token`
keys in `~/.config/yg/config.toml` (standard TOML; the environment wins
over the file).

End to end from a clean checkout:

```sh
docker compose up -d --wait
export YG_BOOTSTRAP_TOKEN=ygt_dev_$(whoami)
cargo run -p yg-cli -- serve --role=all &
export YG_TOKEN=$YG_BOOTSTRAP_TOKEN
cargo run -p yg-cli -- status
cargo run -p yg-cli -- admin repo add https://github.com/octocat/Hello-World
cargo run -p yg-cli -- admin status   # synced commit, then Shard revision + counts once indexed
```

`GET /healthz` (no token) reports per-dependency readiness as bare
`ok`/`error` verdicts (failure detail goes to the server log, never to
anonymous callers); every `/v1` route requires
`Authorization: Bearer $YG_TOKEN`. Member tokens reach the Verbs, MCP, and
the read-only `/v1/status`; `/v1/admin/*` requires the Admin token.
`GET /metrics` serves Prometheus text exposition and requires the Admin token
by default; set `YG_METRICS_UNAUTHENTICATED=true` only when the scrape endpoint
is protected by the deployment network boundary. Metrics are process-local:
`serve --role=all` exposes both API and worker observations; in a split-role
deployment the API endpoint cannot aggregate the separate worker process.
Configure `YG_WORKER_METRICS_ADDR` on each split worker and scrape that endpoint
to collect its job and forge observations.

## Checks

CI runs exactly this on every Change Request, as two jobs; run it locally
before pushing. The cheap job needs no services:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
cargo test --workspace --exclude yg-cli --locked
```

The e2e job runs the `yg-cli` suites against the compose stack (each test
gets its own database, and its object-store keys are prefixed with that
database's name, so parallel tests never share state):

```sh
docker compose up -d --wait postgres minio
docker compose run --rm minio-init
cargo test -p yg-cli --locked
```

The toolchain is pinned in `rust-toolchain.toml` so a new stable clippy can't
redden unrelated Change Requests; bump it deliberately.

## Layout

The cargo workspace follows [RFC 0001 §9](rfc/0001-yggdrasil-v1.md#9-crate-layout):
eight `yg-*` crates under `crates/`.
