#!/usr/bin/env bash
# Full rerun of the Shard profile from a clean checkout. Every step is
# resumable on its own (see each script's header); this just runs them in
# order. Pass --teardown to remove the compose project and caches at the end.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
# Each full rerun writes to a fresh results dir: index_e2e.py and harness.py
# resume from (skip repos already in) whatever results dir they are given.
export RESULTS_DIR=${RESULTS_DIR:-$here/results-$(date -u +%Y%m%dT%H%M%SZ)}
. "$here/env.sh"

cargo build --release --locked -p yg-cli --manifest-path "$REPO_ROOT/Cargo.toml"
CARGO_TARGET_DIR="$REPO_ROOT/target" cargo build --release --locked \
  --manifest-path "$here/harness/Cargo.toml"

compose up -d --wait postgres rustfs
compose run --rm rustfs-init

"$here/serve.sh" start
"$here/run.sh" index_e2e.py      # results/index_e2e.tsv (sequential, ~25 min)
"$here/run.sh" harness.py        # results/harness.jsonl, corpus.tsv (server idle)
"$here/run.sh" shard_sizes.py    # results/shard_sizes.tsv
"$here/dbstat.sh"                # results/dbstat.tsv, edge_columns.tsv
"$here/run.sh" verbs_bench.py    # results/verb_calls.jsonl (restarts the server per Verb)
"$here/run.sh" anatomy.py        # results/anatomy_examples.json (server still up)
"$here/run.sh" edge_page.py      # results/edge_page_<slug>.json
"$here/run.sh" ambiguity.py      # results/ambiguity.tsv (CALLS fan-out per call site)
"$here/run.sh" summarize.py      # results/summary.md
"$here/serve.sh" stop
gzip -9 "$RESULTS_DIR/verb_calls.jsonl"

if [[ "${1:-}" == "--teardown" ]]; then
  compose down -v
  rm -rf "$STATE_DIR"
fi
