# Shared settings for every shard-profile script. Source it: `. ./env.sh`.
# Isolated from the default dev stack: own compose project, own ports, own
# caches under the gitignored data/ dir.

PROFILE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$PROFILE_DIR/../../.." && pwd)"
export PROFILE_DIR REPO_ROOT

export COMPOSE_PROJECT=yg-profile
export YG_POSTGRES_PORT=${YG_POSTGRES_PORT:-25432}
export YG_RUSTFS_PORT=${YG_RUSTFS_PORT:-29000}
export YG_RUSTFS_CONSOLE_PORT=${YG_RUSTFS_CONSOLE_PORT:-29001}
export YG_SERVER_PORT=${YG_SERVER_PORT:-27311}

export STATE_DIR=${STATE_DIR:-$REPO_ROOT/data/shard-profile}
export RESULTS_DIR=${RESULTS_DIR:-$PROFILE_DIR/results}

export YG_DATABASE_URL="postgres://yggdrasil:yggdrasil@localhost:$YG_POSTGRES_PORT/yggdrasil"
export YG_S3_ENDPOINT="http://localhost:$YG_RUSTFS_PORT"
export YG_S3_BUCKET=yggdrasil
export YG_LISTEN="127.0.0.1:$YG_SERVER_PORT"
export YG_GIT_CACHE="$STATE_DIR/git"
export YG_SHARD_CACHE="$STATE_DIR/shard-cache"
# Bounded so the per-repo cold/warm sweep cannot fill a small disk; the sweep
# walks repos one at a time, so LRU eviction never hits a repo mid-measure.
export YG_SHARD_CACHE_MAX_BYTES=${YG_SHARD_CACHE_MAX_BYTES:-4294967296}
export YG_BOOTSTRAP_TOKEN=ygt_profile_bootstrap
# Fixed (not secret: local-only stack) so cursors survive server restarts.
export YG_CURSOR_SECRET=shard-profile-cursor-secret-0123456789abcdef
# Polling and discovery off for the run: no re-index churn, no API budget.
export YG_POLL_INTERVAL=864000
export YG_DISCOVERY_INTERVAL=864000
# Generous rate limit: the latency sweep issues thousands of calls.
export YG_TOKEN_RATE_LIMIT_REQUESTS=1000000
export YG_METRICS_UNAUTHENTICATED=true

export YG_SERVER="http://$YG_LISTEN"
export YG_TOKEN=$YG_BOOTSTRAP_TOKEN
export YG_BIN="$REPO_ROOT/target/release/yg"
export HARNESS_BIN="$REPO_ROOT/target/release/shard-profile-harness"

compose() {
  docker compose -p "$COMPOSE_PROJECT" -f "$REPO_ROOT/compose.yaml" "$@"
}

psql_q() {
  docker exec "${COMPOSE_PROJECT}-postgres-1" psql -U yggdrasil -At -F $'\t' -c "$1"
}
