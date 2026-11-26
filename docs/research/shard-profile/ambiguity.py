"""Where the graph segment's edges come from: CALLS fan-out per call site.

ADR 0006's ambiguity policy emits one CALLS edge per same-named candidate at
every call site. For every published Shard, download graph.sqlite from object
storage (one at a time, deleted after) and measure how many CALLS edges sit
at ambiguous sites (more than one candidate), plus id-string bytes per edge.

Output: results/ambiguity.tsv
Usage: python3 -I ambiguity.py
"""

import os
import sqlite3
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import common  # noqa: E402

QUERIES = {
    "edges": "select count(*) from edges",
    "calls": "select count(*) from edges where kind = 'CALLS'",
    "call_sites": "select count(*) from (select 1 from edges where kind = 'CALLS' group by src, location)",
    "ambiguous_sites": ("select count(*) from (select 1 from edges where kind = 'CALLS'"
                        " group by src, location having count(*) > 1)"),
    "calls_at_ambiguous_sites": ("select coalesce(sum(n), 0) from (select count(*) as n from edges"
                                 " where kind = 'CALLS' group by src, location having n > 1)"),
    "max_candidates_per_site": ("select coalesce(max(n), 0) from (select count(*) as n from edges"
                                " where kind = 'CALLS' group by src, location)"),
    "calls_at_sites_over_10": ("select coalesce(sum(n), 0) from (select count(*) as n from edges"
                               " where kind = 'CALLS' group by src, location having n > 10)"),
    "avg_edge_text_bytes": ("select avg(length(src) + length(dst) + length(kind) + length(provenance)"
                            " + coalesce(length(location), 0)) from edges"),
}


def measure(path):
    conn = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    try:
        return {k: conn.execute(q).fetchone()[0] for k, q in QUERIES.items()}
    finally:
        conn.close()


def main():
    shards = common.psql(
        "select r.slug, s.manifest_key from repos r join shards s on s.id = r.current_shard_id order by r.id")
    rows = []
    for slug, manifest_key in shards:
        key = manifest_key.rsplit("/", 1)[0] + "/graph.sqlite"
        with tempfile.TemporaryDirectory(dir=common.STATE_DIR) as tmp:
            path = os.path.join(tmp, "graph.sqlite")
            with open(path, "wb") as f:
                f.write(common.s3_get(key))
            m = measure(path)
        calls = m["calls"] or 1
        rows.append({"slug": slug, **m,
                     "ambiguous_share_of_calls": f"{m['calls_at_ambiguous_sites'] / calls:.3f}",
                     "calls_per_site": f"{m['calls'] / (m['call_sites'] or 1):.2f}",
                     "avg_edge_text_bytes": f"{m['avg_edge_text_bytes']:.1f}"})
        print(slug, rows[-1]["calls_per_site"], rows[-1]["ambiguous_share_of_calls"], flush=True)
    common.write_tsv(os.path.join(common.RESULTS_DIR, "ambiguity.tsv"), rows, list(rows[0].keys()))


if __name__ == "__main__":
    main()
