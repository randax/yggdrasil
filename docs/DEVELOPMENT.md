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
which is required:

| Variable | Default | Purpose |
|---|---|---|
| `YG_BOOTSTRAP_TOKEN` | — (required) | Bootstrap Admin bearer token |
| `YG_LISTEN` | `127.0.0.1:7311` | Server bind address |
| `YG_DATABASE_URL` | dev compose Postgres | Control-plane database |
| `YG_S3_ENDPOINT` | `http://localhost:9000` | Object storage endpoint |
| `YG_S3_BUCKET` | `yggdrasil` | Shard bucket |
| `YG_S3_ACCESS_KEY` / `YG_S3_SECRET_KEY` | `yggdrasil` / `yggdrasil` | Object storage credentials |
| `YG_S3_REGION` | `us-east-1` | Object storage region |

The CLI talks to a server via `YG_SERVER` (default `http://127.0.0.1:7311`)
and `YG_TOKEN`.

End to end from a clean checkout:

```sh
docker compose up -d --wait
export YG_BOOTSTRAP_TOKEN=ygt_dev_$(whoami)
cargo run -p yg-cli -- serve --role=all &
YG_TOKEN=$YG_BOOTSTRAP_TOKEN cargo run -p yg-cli -- status
```

`GET /healthz` (no token) reports per-dependency readiness; every `/v1`
route requires `Authorization: Bearer $YG_TOKEN`.

## Checks

CI runs exactly this on every Change Request; run it locally before pushing
(the e2e tests need the compose stack up):

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
docker compose up -d --wait
cargo test --workspace --locked
```

The toolchain is pinned in `rust-toolchain.toml` so a new stable clippy can't
redden unrelated Change Requests; bump it deliberately.

## Layout

The cargo workspace follows [RFC 0001 §9](rfc/0001-yggdrasil-v1.md#9-crate-layout):
eight `yg-*` crates under `crates/`.
