"""Why is a hub's first `neighbors` page so large? Count its edges by kind,
by whether they touch the origin, and by (src, dst) multiplicity.

Output: results/edge_page_<slug>.json
Usage: ANATOMY_SLUG=etcd-io/etcd python3 -I edge_page.py   (server must be up)
"""

import collections
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import common  # noqa: E402
import verbs_bench  # noqa: E402


def main():
    slug = os.environ.get("ANATOMY_SLUG", "etcd-io/etcd")
    with open(os.path.join(common.RESULTS_DIR, "harness.jsonl")) as f:
        report = next(json.loads(line) for line in f if json.loads(line)["slug"] == slug)
    body = dict(verbs_bench.calls_for("neighbors", report))["neighbors.top_any.d1"]
    status, data = common.post_verb("/v1/verbs/neighbors", body)
    page = json.loads(data)
    origin = body["id"]
    edges = page["edges"]
    pairs = collections.Counter((e["src"], e["dst"], e["kind"]) for e in edges)
    out = {
        "slug": slug, "origin": origin, "status": status, "bytes": len(data),
        "nodes": len(page["nodes"]), "edges": len(edges),
        "edges_by_kind": collections.Counter(e["kind"] for e in edges),
        "edges_touching_origin": sum(origin in (e["src"], e["dst"]) for e in edges),
        "distinct_src_dst_kind": len(pairs),
        "max_parallel_edges_one_pair": max(pairs.values()) if pairs else 0,
        "edges_with_location": sum(1 for e in edges if e.get("location")),
        "mean_edge_json_bytes": round(sum(len(json.dumps(e)) for e in edges) / max(len(edges), 1), 1),
        "example_edge": edges[0] if edges else None,
    }
    with open(os.path.join(common.RESULTS_DIR, f"edge_page_{slug.replace('/', '_')}.json"), "w") as f:
        json.dump(out, f, indent=1, sort_keys=True)
    print(json.dumps(out, indent=1, sort_keys=True))


if __name__ == "__main__":
    main()
