"""Shard sizes as object storage holds them (RustFS ListObjectsV2), joined
with node/edge counts from Postgres and LOC from corpus.tsv.

Output: results/shard_sizes.tsv
Usage: python3 -I shard_sizes.py
"""

import os
import sys
import xml.etree.ElementTree as ET

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import common  # noqa: E402

NS = {"s3": "http://s3.amazonaws.com/doc/2006-03-01/"}


def list_bucket():
    sizes = {}
    token = None
    while True:
        query = "?list-type=2&max-keys=1000"
        if token:
            from urllib.parse import quote
            query += "&continuation-token=" + quote(token, safe="")
        root = ET.fromstring(common.s3_get("", query))
        for item in root.findall("s3:Contents", NS):
            sizes[item.find("s3:Key", NS).text] = int(item.find("s3:Size", NS).text)
        if root.findtext("s3:IsTruncated", default="false", namespaces=NS) != "true":
            return sizes
        token = root.findtext("s3:NextContinuationToken", namespaces=NS)


def main():
    sizes = list_bucket()
    shards = common.psql(
        "select r.slug, r.id, s.manifest_key, s.node_count, s.edge_count"
        " from repos r join shards s on s.id = r.current_shard_id order by r.id"
    )
    corpus = {r["slug"]: r for r in common.read_tsv(os.path.join(common.PROFILE_DIR, "corpus.tsv"))}
    langs = {r["slug"]: r["language"] for r in common.corpus()}
    rows = []
    for slug, repo_id, manifest_key, nodes, edges in shards:
        prefix = manifest_key.rsplit("/", 1)[0]
        graph = sizes.get(f"{prefix}/graph.sqlite", 0)
        fts = sizes.get(f"{prefix}/fts.tar", 0)
        manifest = sizes.get(manifest_key, 0)
        total = graph + fts + manifest
        nodes, edges = int(nodes), int(edges)
        loc = int(corpus[slug]["loc_six_languages"]) if slug in corpus else 0
        tree = int(corpus[slug]["tree_bytes"]) if slug in corpus else 0
        rows.append({
            "slug": slug, "language": langs.get(slug, ""), "repo_id": repo_id,
            "nodes": nodes, "edges": edges, "loc_six": loc, "tree_bytes": tree,
            "graph_bytes": graph, "fts_bytes": fts, "manifest_bytes": manifest, "total_bytes": total,
            "fts_share": f"{fts / total:.3f}" if total else "",
            "total_b_per_edge": f"{total / edges:.1f}" if edges else "",
            "graph_b_per_edge": f"{graph / edges:.1f}" if edges else "",
            "graph_b_per_node": f"{graph / nodes:.1f}" if nodes else "",
            "total_b_per_loc": f"{total / loc:.1f}" if loc else "",
            "fts_b_per_tree_byte": f"{fts / tree:.3f}" if tree else "",
        })
    fields = list(rows[0].keys()) if rows else []
    common.write_tsv(os.path.join(common.RESULTS_DIR, "shard_sizes.tsv"), rows, fields)
    print(f"{len(rows)} shards; bucket holds {len(sizes)} objects, {sum(sizes.values())} bytes")


if __name__ == "__main__":
    main()
