#!/usr/bin/env bash
# End-to-end smoke of the reference deployment profile: the server runs
# FROM THE IMAGE (never a local cargo build), registers a public repo,
# indexes it, and answers a search Verb. CI runs this against the image
# it just built; operators can run it against any tag via YG_IMAGE.
set -euo pipefail

readonly PROJECT="${YG_SMOKE_PROJECT:-yggdrasil-deploy-smoke}"
readonly REPO_URL=https://github.com/octocat/Hello-World
readonly REPO_QUALIFIER=github.com/octocat/Hello-World

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

export YG_IMAGE="${YG_IMAGE:?set YG_IMAGE to the image tag under test}"
export YG_BOOTSTRAP_TOKEN="${YG_BOOTSTRAP_TOKEN:-deploy-smoke-bootstrap-token}"
export YG_CURSOR_SECRET="${YG_CURSOR_SECRET:-deploy-smoke-cursor-secret-32-bytes-min}"
# Everything talks over the compose network; ephemeral host ports avoid
# colliding with a developer's running dev stack.
export YG_POSTGRES_PORT=0
export YG_MINIO_PORT=0
export YG_MINIO_CONSOLE_PORT=0
export YG_SERVER_PORT=0

COMPOSE=(
  docker compose
  --project-name "$PROJECT"
  --project-directory "$repo_root"
  --file "$repo_root/compose.yaml"
)

cleanup() {
  "${COMPOSE[@]}" --profile deployment down --volumes --remove-orphans >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup

"${COMPOSE[@]}" up -d --wait postgres minio
"${COMPOSE[@]}" run --rm minio-init
"${COMPOSE[@]}" --profile deployment up -d --no-build --no-deps --wait server

yg() {
  "${COMPOSE[@]}" --profile deployment exec -T \
    -e YG_SERVER=http://127.0.0.1:7311 \
    -e "YG_TOKEN=$YG_BOOTSTRAP_TOKEN" \
    server /usr/local/bin/yg "$@"
}

yg admin repo add "$REPO_URL" --depth 1

status_json=
for _ in $(seq 90); do
  status_json="$(yg admin status --json)"
  if grep -Eq '"index":\{[^}]*"state":"indexed"' <<<"$status_json" &&
    grep -Fq '"shard":{' <<<"$status_json"; then
    break
  fi
  sleep 2
done

if ! grep -Eq '"index":\{[^}]*"state":"indexed"' <<<"$status_json"; then
  printf 'repository did not finish indexing:\n%s\n' "$status_json" >&2
  "${COMPOSE[@]}" --profile deployment logs server >&2
  exit 1
fi

search_json="$(yg search 'Hello World' --repo "$REPO_QUALIFIER" --json)"
if grep -Fq '"hits":[]' <<<"$search_json" ||
  ! grep -Fq "\"repo\":\"$REPO_QUALIFIER\"" <<<"$search_json"; then
  printf 'search Verb returned no matching hit:\n%s\n' "$search_json" >&2
  exit 1
fi

printf 'deployment smoke passed:\n%s\n' "$search_json"
