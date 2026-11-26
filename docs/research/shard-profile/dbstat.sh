#!/usr/bin/env bash
# dbstat breakdown (bytes by table/index) of the graph segments harness.py
# kept under $STATE_DIR/graphs/<repo_id>.sqlite (KEEP_GRAPHS), plus the
# per-column payload bytes of the edges table, which dbstat cannot split.
# Output: results/dbstat.tsv, results/edge_columns.tsv
set -euo pipefail
. "$(dirname "$0")/env.sh"
out="$RESULTS_DIR/dbstat.tsv"
cols="$RESULTS_DIR/edge_columns.tsv"
printf 'repo_id\tname\tpages\tpgsize_bytes\tpayload_bytes\tunused_bytes\n' >"$out"
printf 'repo_id\tedges\tsrc_bytes\tdst_bytes\tkind_bytes\tprovenance_bytes\tlocation_bytes\tnull_locations\n' >"$cols"
for db in "$STATE_DIR"/graphs/*.sqlite; do
  id=$(basename "$db" .sqlite)
  sqlite3 -readonly -separator $'\t' "$db" \
    "SELECT '$id', name, count(*), sum(pgsize), sum(payload), sum(unused)
       FROM dbstat GROUP BY name ORDER BY sum(pgsize) DESC;" >>"$out"
  sqlite3 -readonly -separator $'\t' "$db" \
    "SELECT '$id', count(*), sum(length(src)), sum(length(dst)), sum(length(kind)),
            sum(length(provenance)), sum(coalesce(length(location), 0)),
            sum(location IS NULL) FROM edges;" >>"$cols"
done
column -t -s $'\t' "$out"
column -t -s $'\t' "$cols"
