# Deploying yggdrasil

The reference deployment is one `yg serve --role=all` container with Postgres
for control-plane state and MinIO for Shard objects. It is intended as a
single-host starting point. Put TLS and any public access control in a reverse
proxy in front of port 7311.

## Image

The root `Dockerfile` builds the `yg` release binary with the Rust 1.96.0
toolchain pinned by `rust-toolchain.toml`. The runtime is
`debian:bookworm-slim`, runs as the numeric non-root user `10001:10001`, and
contains only the runtime tools the combined role needs: Git, tar, and CA
certificates. The image defaults to `yg serve --role=all`.

Build it locally:

```sh
docker build --tag yggdrasil:local .
```

## First boot with Compose

The `deployment` profile adds the server without changing the development
services. Generate and retain two independent secrets before starting it:

```sh
export YG_BOOTSTRAP_TOKEN="$(openssl rand -hex 32)"
export YG_CURSOR_SECRET="$(openssl rand -hex 32)"
docker compose up -d --wait postgres minio
docker compose run --rm minio-init
docker compose --profile deployment up -d --no-deps --wait server
```

`YG_BOOTSTRAP_TOKEN` is the initial Admin bearer token. Treat it like a
password. `YG_CURSOR_SECRET` signs pagination cursors, must contain at least 32
bytes of high-entropy material, and must remain stable across restarts. The
server deliberately refuses to start when either variable is empty or invalid.
To index private repositories or run org discovery, also export
`YG_GITHUB_TOKEN` before starting the profile — the service forwards it to
the workers; without it only public repositories clone.

> **Secret handling:** the profile passes these secrets as container
> environment variables, so anyone who can run `docker inspect` or render
> the compose config on the host can read them — never paste rendered
> configs into logs or issues. For a production deployment, prefer a
> secret store (Docker/Compose `secrets:`, or your orchestrator's
> equivalent) over host environment variables; this reference profile
> favors first-boot simplicity.
The complete configuration table, including object-store, cache, protection,
polling, and GC settings, is in [Development](DEVELOPMENT.md#running-the-index-server).

The profile wires the container to `postgres:5432` and `minio:9000` on the
Compose network and publishes the API at `http://127.0.0.1:7311`. Override the
host port with `YG_SERVER_PORT`. To use a prebuilt image rather than the local
build, set `YG_IMAGE` to its immutable tag or digest and use `--no-build`.

CI exercises this exact path on every change: `scripts/deployment-smoke.sh`
boots the profile from the freshly built image, registers a public
repository, waits for it to index, and asserts a search Verb answers.
Run it against any image with `YG_IMAGE=<tag> scripts/deployment-smoke.sh`.

> **Network boundary:** this profile intentionally reuses the unchanged local
> development services. Their existing port mappings publish Postgres 5432,
> the MinIO API 9000, and the MinIO console 9001 on all host interfaces with
> development-only credentials. Do not expose those ports to an untrusted
> network. Enforce a host firewall or an equivalent trusted-network boundary;
> for a production platform, translate this topology to private backing-service
> networks and replace every database/object-store credential.

The container healthcheck runs `yg status --json` against the authenticated
status route. `GET /healthz` remains available without authentication and
reports readiness of Postgres and object storage. All `/v1` routes require a
bearer token.

### Register, index, and query

The image also contains the CLI, so the first walkthrough needs no host Rust
toolchain. Register a small public repository with a shallow clone:

```sh
docker compose --profile deployment exec \
  -e YG_SERVER=http://127.0.0.1:7311 \
  -e YG_TOKEN="$YG_BOOTSTRAP_TOKEN" \
  server yg admin repo add https://github.com/octocat/Hello-World --depth 1
```

Indexing is asynchronous. Repeat the status command until the repository row
includes `shard ... (N nodes, N edges)`:

```sh
docker compose --profile deployment exec \
  -e YG_SERVER=http://127.0.0.1:7311 \
  -e YG_TOKEN="$YG_BOOTSTRAP_TOKEN" \
  server yg admin status
```

Then query the `search` Verb:

```sh
docker compose --profile deployment exec \
  -e YG_SERVER=http://127.0.0.1:7311 \
  -e YG_TOKEN="$YG_BOOTSTRAP_TOKEN" \
  server yg search Hello --repo github.com/octocat/Hello-World --json
```

## Persistent data and backups

These named volumes must survive container replacement:

| Volume | Contents | Required for recovery |
|---|---|---|
| `postgres-data` | registrations, job state, tokens, and current Shard pointers | Yes; back up consistently |
| `minio-data` | authoritative Shard objects | Yes; back up consistently with Postgres |
| `server-git-cache` | worker cache of bare Git clones | No; speeds later syncs |
| `server-shard-cache` | API cache of downloaded Shards | No; rebuilt from MinIO |

Compose prefixes volume names with the project name (`yggdrasil-dev` by
default). Never run `docker compose down --volumes` against a deployment whose
data you intend to retain. Back up Postgres and MinIO as one recovery point so
database Shard pointers cannot outpace the stored objects.

## Upgrade

1. Back up `postgres-data` and `minio-data` together.
2. Set `YG_IMAGE` to the new immutable image tag or digest, then pull it with
   `docker compose --profile deployment pull server`.
3. Recreate only the application container with
   `docker compose --profile deployment up -d --no-build --no-deps --wait server`.
4. Check `docker compose logs server`, `GET /healthz`, and `yg status`.

`yg-control` embeds SQLx migrations in the binary.
`ControlPlane::connect_and_migrate` runs during server boot; applied versions
are recorded in Postgres `_sqlx_migrations`, so a restart against an
up-to-date database is a no-op. Do not roll the binary back across a schema
change unless that release explicitly documents rollback compatibility.

To stop the deployment without deleting data, run:

```sh
docker compose --profile deployment down
```
