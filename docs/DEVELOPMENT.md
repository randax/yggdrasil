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
full community console; the S3 API is what yggdrasil actually needs.

If a default host port collides with something already running, override it:
`YG_POSTGRES_PORT`, `YG_MINIO_PORT`, `YG_MINIO_CONSOLE_PORT` (e.g.
`YG_POSTGRES_PORT=15432 docker compose up -d`).

These credentials are for local development only. Data persists in named
volumes (`yggdrasil-dev_postgres-data`, `yggdrasil-dev_minio-data`);
`docker compose down -v` resets everything.

## Checks

CI runs exactly this on every Change Request; run it locally before pushing:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
cargo test --workspace --locked
```

The toolchain is pinned in `rust-toolchain.toml` so a new stable clippy can't
redden unrelated Change Requests; bump it deliberately.

## Layout

The cargo workspace follows [RFC 0001 §9](rfc/0001-yggdrasil-v1.md#9-crate-layout):
eight `yg-*` crates under `crates/`.
