#!/usr/bin/env bash
# Run one shard-profile Python step with the shared environment:
#   ./run.sh index_e2e.py [slug ...]
set -euo pipefail
. "$(dirname "$0")/env.sh"
mkdir -p "$RESULTS_DIR" "$STATE_DIR"
script=$1
shift
exec python3 -I "$PROFILE_DIR/$script" "$@"
