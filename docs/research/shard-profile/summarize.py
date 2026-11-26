"""Turn the raw results into the Markdown tables the report quotes.

Inputs: results/{index_e2e.tsv,harness.jsonl,shard_sizes.tsv,verb_calls.jsonl[.gz]}
Output: results/summary.md (+ results/verb_summary.tsv, results/fanout.tsv)
Usage: python3 -I summarize.py
Size classes use LOC in the six indexed languages (tokei code lines):
S < 10k, M 10k-300k, L >= 300k.
"""

import gzip
import json
import os
import statistics
import sys
from collections import defaultdict

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import common  # noqa: E402

R = common.RESULTS_DIR
P = common.percentile


def size_class(loc):
    loc = int(loc)
    return "S" if loc < 10_000 else ("M" if loc < 300_000 else "L")


def mb(b):
    return f"{int(b) / 1e6:.1f}"


def fmt(x, nd=1):
    return "" if x is None else f"{x:.{nd}f}"


def table(header, rows):
    out = ["| " + " | ".join(header) + " |", "|" + "---|" * len(header)]
    out += ["| " + " | ".join(str(c) for c in row) + " |" for row in rows]
    return "\n".join(out)


def load():
    harness = {}
    with open(os.path.join(R, "harness.jsonl")) as f:
        for line in f:
            if line.strip():
                h = json.loads(line)
                harness[h["slug"]] = h
    sizes = {r["slug"]: r for r in common.read_tsv(os.path.join(R, "shard_sizes.tsv"))}
    e2e = {r["slug"]: r for r in common.read_tsv(os.path.join(R, "index_e2e.tsv"))}
    calls = []
    path = os.path.join(R, "verb_calls.jsonl")
    if os.path.exists(path):
        with open(path) as f:
            calls = [json.loads(line) for line in f if line.strip()]
    elif os.path.exists(path + ".gz"):  # the committed copy is gzipped
        with gzip.open(path + ".gz", "rt") as f:
            calls = [json.loads(line) for line in f if line.strip()]
    order = [c["slug"] for c in common.corpus() if c["slug"] in harness]
    return order, harness, sizes, e2e, calls


def sizes_section(order, harness, sizes):
    rows, per_edge, graph_share = [], [], []
    for slug in order:
        s, h = sizes[slug], harness[slug]
        loc = h["loc"]["code_six"]
        total = int(s["total_bytes"])
        per_edge.append(float(s["total_b_per_edge"]))
        graph_share.append(int(s["graph_bytes"]) / total)
        rows.append([slug, s["language"], size_class(loc), f"{loc:,}", f"{int(s['nodes']):,}",
                     f"{int(s['edges']):,}", mb(s["graph_bytes"]), mb(s["fts_bytes"]), mb(total),
                     f"{100 * int(s['graph_bytes']) / total:.0f}%", s["total_b_per_edge"],
                     s["graph_b_per_node"], s["total_b_per_loc"]])
    head = ["repo", "lang", "class", "LOC", "nodes", "edges", "graph MB", "FTS MB", "total MB",
            "graph share", "B/edge", "graph B/node", "B/LOC"]
    summary = (f"Total B/edge: median {statistics.median(per_edge):.0f}, range "
               f"{min(per_edge):.0f}-{max(per_edge):.0f}. Graph share of Shard bytes: median "
               f"{100 * statistics.median(graph_share):.0f}%, range {100 * min(graph_share):.0f}%-"
               f"{100 * max(graph_share):.0f}%. Corpus total: "
               f"{sum(int(sizes[s]['total_bytes']) for s in order) / 1e9:.2f} GB.")
    return table(head, rows) + "\n\n" + summary


def edge_kind_section(order, harness):
    totals = defaultdict(int)
    for slug in order:
        for kind, n in harness[slug]["kinds"]["edges"].items():
            totals[kind] += n
    all_edges = sum(totals.values())
    rows = [[k, f"{n:,}", f"{100 * n / all_edges:.1f}%"] for k, n in
            sorted(totals.items(), key=lambda kv: -kv[1])]
    node_totals = defaultdict(int)
    for slug in order:
        for kind, n in harness[slug]["kinds"]["nodes"].items():
            node_totals[kind] += n
    all_nodes = sum(node_totals.values())
    nrows = [[k, f"{n:,}", f"{100 * n / all_nodes:.1f}%"] for k, n in
             sorted(node_totals.items(), key=lambda kv: -kv[1])]
    return (table(["edge kind", "edges (corpus)", "share"], rows) + "\n\n" +
            table(["node kind", "nodes (corpus)", "share"], nrows))


STAGES = ["extract_tree", "git_log_only", "history_walk", "syntactic_pass", "merge_history",
          "build_fts_alone", "graph_build_estimate", "prepare_shard", "write_local_fs"]


def stage(h, name):
    if name == "graph_build_estimate":
        return h["graph_build_estimate_s"]
    return h["timings_s"][name]


def index_section(order, harness, e2e):
    rows, per_kloc, stage_sum = [], [], defaultdict(float)
    for slug in order:
        h, e = harness[slug], e2e[slug]
        loc = h["loc"]["code_six"]
        idx = float(e["fetch_done_to_published_s"] or 0)
        if loc:
            per_kloc.append(1000 * idx / (loc / 1000))
        pipeline = sum(stage(h, s) for s in ("extract_tree", "history_walk", "syntactic_pass",
                                              "merge_history", "prepare_shard", "write_local_fs"))
        for s in STAGES:
            stage_sum[s] += stage(h, s)
        slowest = max(("extract_tree", "history_walk", "syntactic_pass", "merge_history",
                       "prepare_shard", "write_local_fs"), key=lambda s: stage(h, s))
        rows.append([slug, size_class(loc), f"{loc:,}", e["fetch_job_s"], e["fetch_done_to_published_s"],
                     fmt(1000 * idx / (loc / 1000)) if loc else "",
                     *[fmt(stage(h, s), 2) for s in STAGES], fmt(pipeline, 2), slowest,
                     f"{h['max_rss_bytes'] / 1e6:.0f}"])
    head = ["repo", "class", "LOC", "e2e fetch s", "e2e fetch-done to published s", "e2e ms/kLOC",
            "extract", "git log", "history walk", "syntactic", "merge", "FTS build (alone)",
            "graph build (est.)", "prepare_shard", "write FS", "harness pipeline s", "slowest pass",
            "harness peak RSS MB"]
    total = sum(stage_sum[s] for s in ("extract_tree", "history_walk", "syntactic_pass",
                                       "merge_history", "prepare_shard", "write_local_fs"))
    share = ", ".join(f"{s} {100 * stage_sum[s] / total:.0f}%" for s in
                      ("extract_tree", "history_walk", "syntactic_pass", "merge_history",
                       "prepare_shard", "write_local_fs"))
    split = (f"Inside prepare_shard (corpus sums): FTS build {stage_sum['build_fts_alone']:.1f}s, "
             f"graph build (est.) {stage_sum['graph_build_estimate']:.1f}s.")
    summary = (f"E2E index time (fetch done to Shard published) per kLOC: median "
               f"{statistics.median(per_kloc):.0f} ms, range {min(per_kloc):.0f}-{max(per_kloc):.0f} ms. "
               f"Harness pipeline share of corpus time: {share}. {split}")
    return table(head, rows) + "\n\n" + summary


def verb_section(calls):
    by = defaultdict(list)
    cold = defaultdict(list)
    for c in calls:
        if c.get("status") != 200 or c["phase"] == "fanout" or c["slug"] == "*all*":
            continue
        cls = size_class(c["loc_six"])
        if c["phase"] == "warm":
            by[(c["verb"], cls)].append(c["ms"])
        elif c["phase"] == "cold":
            cold[(c["verb"], cls)].append(c["ms"])
    rows = []
    for verb in ("node", "neighbors", "search", "history"):
        for cls in ("S", "M", "L"):
            w = by.get((verb, cls), [])
            k = cold.get((verb, cls), [])
            if not w:
                continue
            rows.append([verb, cls, len(k), fmt(P(k, 50)), fmt(max(k)) if k else "", len(w),
                         fmt(P(w, 50), 2), fmt(P(w, 95), 2), fmt(max(w), 1)])
    head = ["verb", "class", "cold n (repos)", "cold p50 ms", "cold max ms", "warm n",
            "warm p50 ms", "warm p95 ms", "warm max ms"]
    return table(head, rows)


def per_call_section(calls):
    by = defaultdict(list)
    sizes = defaultdict(list)
    for c in calls:
        if c.get("status") != 200 or c["phase"] != "warm":
            continue
        key = c["call"] if c["verb"] != "neighbors" else c["call"]
        by[key].append(c["ms"])
        if c.get("rep") in (0, None):
            sizes[key].append(c["bytes"])
    rows = []
    for key in sorted(by):
        b = sizes.get(key) or [0]
        rows.append([key, len(by[key]), fmt(P(by[key], 50), 2), fmt(P(by[key], 95), 2),
                     fmt(max(by[key]), 1), int(statistics.median(b)), int(max(b)),
                     int(statistics.median(b) / 4)])
    head = ["call", "warm n", "p50 ms", "p95 ms", "max ms", "median bytes", "max bytes",
            "median tokens (bytes/4)"]
    return table(head, rows)


def anatomy_section(calls):
    by = defaultdict(lambda: [0, 0, 0, 0])
    for c in calls:
        if c.get("status") != 200 or c.get("phase") != "warm" or c.get("rep") != 0:
            continue
        a = by[c["verb"]]
        a[0] += c["bytes"]
        a[1] += c.get("id_string_bytes", 0)
        a[2] += c.get("next_cursor_bytes", 0)
        a[3] += 1
    rows = [[v, a[3], f"{a[0] / a[3]:.0f}", f"{100 * a[1] / a[0]:.0f}%", f"{100 * a[2] / a[0]:.0f}%"]
            for v, a in sorted(by.items())]
    return table(["verb", "responses", "mean bytes", "node-id strings", "next_cursor"], rows)


def fanout_section(calls):
    rows = []
    worst = {}
    for c in calls:
        if c["phase"] != "fanout":
            continue
        key = (c["slug"], c["call"])
        rows.append(c)
        if c["slug"] not in worst or c.get("bytes", 0) > worst[c["slug"]].get("bytes", 0):
            worst[c["slug"]] = c
    warm = defaultdict(list)
    first_bytes = {}
    for c in calls:
        if c["verb"] == "neighbors" and c["phase"] == "warm" and c.get("status") == 200:
            warm[(c["slug"], c["call"])].append(c["ms"])
            first_bytes[(c["slug"], c["call"])] = c["bytes"]
    common.write_tsv(os.path.join(R, "fanout.tsv"), [
        {**{k: c.get(k) for k in ("slug", "call", "pages", "nodes", "edges", "bytes", "ms",
                                  "truncated", "exhausted", "status")},
         "first_page_warm_p50_ms": fmt(P(warm[(c["slug"], c["call"])], 50), 2),
         "first_page_bytes": first_bytes.get((c["slug"], c["call"]))} for c in rows],
        ["slug", "call", "pages", "nodes", "edges", "bytes", "ms", "truncated", "exhausted",
         "status", "first_page_warm_p50_ms", "first_page_bytes"])
    out = []
    for slug, c in worst.items():
        out.append([slug, c["call"], c.get("pages"), c.get("nodes"), c.get("edges"),
                    f"{c.get('bytes', 0) / 1e6:.2f}", f"{c.get('ms', 0):.0f}",
                    c.get("truncated"), first_bytes.get((slug, c["call"])),
                    fmt(P(warm[(slug, c["call"])], 50), 1)])
    head = ["repo", "worst variant (by total bytes)", "pages", "nodes", "edges", "total MB",
            "total ms (all pages)", "truncated", "first-page bytes", "first-page warm p50 ms"]
    return table(head, out)


def main():
    order, harness, sizes, e2e, calls = load()
    parts = [
        "## Shard sizes\n\n" + sizes_section(order, harness, sizes),
        "## Edge and node kinds\n\n" + edge_kind_section(order, harness),
        "## Index time\n\n" + index_section(order, harness, e2e),
    ]
    if calls:
        parts += [
            "## Verb latency by size class\n\n" + verb_section(calls),
            "## Per call\n\n" + per_call_section(calls),
            "## Payload anatomy\n\n" + anatomy_section(calls),
            "## neighbors fan-out (worst variant per repo)\n\n" + fanout_section(calls),
        ]
    with open(os.path.join(R, "summary.md"), "w") as f:
        f.write("\n\n".join(parts) + "\n")
    print(f"wrote {os.path.join(R, 'summary.md')}")


if __name__ == "__main__":
    main()
