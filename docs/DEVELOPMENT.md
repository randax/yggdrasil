# Developing yggdrasil

## Prerequisites

- Rust (stable — `rust-toolchain.toml` pins the channel and pulls in rustfmt + clippy)
- Docker with the compose plugin

## Dev services

`docker compose up -d` starts the two backing services the Index Server needs:

| Service | Image | Endpoint | Credentials |
|---|---|---|---|
| Postgres (control plane) | `postgres:17` | `localhost:5432`, database `yggdrasil` | `yggdrasil` / `yggdrasil` |
| MinIO S3 API (Shard storage) | `minio/minio` | `http://localhost:9000` | `yggdrasil` / `yggdrasil` |
| MinIO console | — | `http://localhost:9001` | `yggdrasil` / `yggdrasil` |

Connection string: `postgres://yggdrasil:yggdrasil@localhost:5432/yggdrasil`

These credentials are for local development only. Data persists in named
volumes (`yggdrasil-dev_postgres-data`, `yggdrasil-dev_minio-data`);
`docker compose down -v` resets everything.

## Checks

CI runs exactly this on every change request; run it locally before pushing:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Layout

The cargo workspace follows [RFC 0001 §9](rfc/0001-yggdrasil-v1.md#9-crate-layout):
eight `yg-*` crates under `crates/`.
