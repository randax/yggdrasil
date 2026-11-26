"""Cold and warm Verb latency, payload bytes, and `neighbors` fan-out.

One pass per Verb. Each pass restarts `yg serve` with an empty Shard cache
dir (serve.sh start --cold), then walks the corpus repo by repo: the first
call of the pass on a repo is that repo's *cold* sample (its Shard segment
must come from object storage); every call is then repeated WARM_REPS times
for the warm sample. The neighbors pass also follows next_cursor to the end
for each fan-out variant (the `fanout` phase). Calls go over one keep-alive
HTTP/1.1 connection; latency is client-observed wall time.

Inputs: results/harness.jsonl (hub picks per repo)
Output: results/verb_calls.jsonl (one line per call), results/metrics-<verb>.txt
Usage: python3 -I verbs_bench.py [verb ...]
"""

import http.client
import json
import os
import subprocess
import sys
import time
import urllib.parse

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import common  # noqa: E402

WARM_REPS = int(os.environ.get("WARM_REPS", "20"))
MAX_PAGES = int(os.environ.get("MAX_PAGES", "200"))
FORGE_HOST = "github" + ".com"


class Client:
    def __init__(self):
        url = urllib.parse.urlparse(common.SERVER)
        self.host, self.port = url.hostname, url.port
        self.conn = None

    def post(self, verb, body):
        payload = json.dumps(body).encode()
        headers = {"Authorization": f"Bearer {common.TOKEN}", "Content-Type": "application/json"}
        for attempt in range(2):
            if self.conn is None:
                self.conn = http.client.HTTPConnection(self.host, self.port, timeout=120)
            try:
                start = time.perf_counter()
                self.conn.request("POST", f"/v1/verbs/{verb}", payload, headers)
                resp = self.conn.getresponse()
                data = resp.read()
                return resp.status, data, (time.perf_counter() - start) * 1000
            except (http.client.HTTPException, ConnectionError, OSError):
                self.conn.close()
                self.conn = None
                if attempt:
                    raise
        raise AssertionError("unreachable")


def public_id(qualifier, local_id):
    prefix, rest = local_id.split(":", 1)
    return f"{prefix}:{qualifier}:{rest}"


def calls_for(verb, report):
    """(call name, body) pairs for one repo; bodies use public ids."""
    q = f"{FORGE_HOST}/{report['slug']}"
    h = report["hubs"]
    pid = lambda local: public_id(q, local)  # noqa: E731
    sym = h["top_symbol"][0] if h["top_symbol"] else None
    med = h["median_symbol"][0] if h["median_symbol"] else None
    out = []
    if verb == "node":
        for name, local in [("node.top_symbol", sym), ("node.median_symbol", med),
                            ("node.top_file", h["top_file"] and h["top_file"][0]),
                            ("node.top_any", h["top_any"][0][0])]:
            if local:
                out.append((name, {"id": pid(local)}))
    elif verb == "search":
        common_name = h["common_name"][0] if h["common_name"] else "main"
        sym_name = h["top_symbol"][2] if h["top_symbol"] else "main"
        out += [
            ("search.common_name", {"query": common_name, "repos": [q]}),
            ("search.top_symbol_name", {"query": sym_name, "repos": [q]}),
            ("search.word_error", {"query": "error", "repos": [q]}),
            ("search.common_name_l100", {"query": common_name, "repos": [q], "limit": 100}),
        ]
    elif verb == "history":
        if h["hot_file"]:
            out += [("history.hot_file", {"id": pid(h["hot_file"][0])}),
                    ("history.hot_file_l1000", {"id": pid(h["hot_file"][0]), "limit": 1000})]
        if med:
            out.append(("history.median_symbol", {"id": pid(med)}))
    elif verb == "neighbors":
        if med:
            out.append(("neighbors.median_symbol.d1", {"id": pid(med)}))
        hubs = [("top_any", h["top_any"][0][0]), ("top_symbol", sym),
                ("top_file", h["top_file"] and h["top_file"][0]),
                ("top_package", h["top_package"] and h["top_package"][0])]
        for hub_name, local in hubs:
            if not local:
                continue
            for variant, extra in NEIGHBOR_VARIANTS:
                out.append((f"neighbors.{hub_name}.{variant}", {"id": pid(local), **extra}))
    return out


NEIGHBOR_VARIANTS = [
    ("d1", {}),
    ("d1.l1000", {"limit": 1000}),
    ("d1.out", {"direction": "out"}),
    ("d1.in", {"direction": "in"}),
    ("d1.calls", {"edge_kinds": ["CALLS"]}),
    ("d2", {"depth": 2}),
    ("d3.l1000", {"depth": 3, "limit": 1000}),
]


def shape(verb, status, data):
    """Counts and payload anatomy for one response."""
    info = {"status": status, "bytes": len(data), "approx_tokens": len(data) // 4}
    try:
        body = json.loads(data)
    except ValueError:
        return info
    if status != 200:
        info["error"] = data[:300].decode(errors="replace")
        return info
    cursor = body.get("next_cursor") or ""
    info["next_cursor_bytes"] = len(cursor)
    info["has_next"] = bool(cursor)
    info["truncated"] = body.get("truncated")
    if verb == "search":
        info["items"] = len(body.get("hits", []))
    elif verb == "history":
        info["items"] = len(body.get("commits", []))
    elif verb == "neighbors":
        info["items"] = len(body.get("nodes", []))
        info["edges"] = len(body.get("edges", []))
    elif verb == "node":
        info["items"] = sum(e.get("count", 0) for side in ("in", "out")
                            for e in body.get("edges", {}).get(side, []))
    info["id_string_bytes"] = id_bytes(body)
    return info


def id_bytes(value):
    """Bytes spent on node-id strings (`kind:qualifier:local`) in a response."""
    if isinstance(value, dict):
        return sum(id_bytes(v) for v in value.values())
    if isinstance(value, list):
        return sum(id_bytes(v) for v in value)
    if isinstance(value, str) and f":{FORGE_HOST}/" in value:
        return len(value.encode()) + 2
    return 0


def fanout(client, verb, body):
    """Follow next_cursor to exhaustion; return page stats."""
    pages, total_bytes, total_ms, items, edges, truncated = 0, 0, 0.0, 0, 0, False
    current = dict(body)
    while pages < MAX_PAGES:
        status, data, ms = client.post(verb, current)
        pages += 1
        total_bytes += len(data)
        total_ms += ms
        if status != 200:
            return {"pages": pages, "status": status, "bytes": total_bytes, "ms": total_ms}
        page = json.loads(data)
        items += len(page.get("nodes", []))
        edges += len(page.get("edges", []))
        truncated = truncated or bool(page.get("truncated"))
        cursor = page.get("next_cursor")
        if not cursor:
            break
        current = {"id": body["id"], "cursor": cursor}
    return {"pages": pages, "status": 200, "bytes": total_bytes, "ms": round(total_ms, 3),
            "nodes": items, "edges": edges, "truncated": truncated,
            "exhausted": pages < MAX_PAGES}


def scrape_metrics(verb):
    with open(os.path.join(common.RESULTS_DIR, f"metrics-{verb}.txt"), "wb") as f:
        f.write(subprocess.run(["curl", "-fsS", f"{common.SERVER}/metrics"],
                               capture_output=True).stdout)


def run_pass(verb, reports, sink):
    subprocess.run([os.path.join(common.PROFILE_DIR, "serve.sh"), "start", "--cold"], check=True)
    client = Client()
    subprocess.run(["curl", "-fsS", "-o", "/dev/null", "-H", f"Authorization: Bearer {common.TOKEN}",
                    f"{common.SERVER}/v1/status"], check=True)
    for report in reports:
        base = {"pass": verb, "verb": verb, "slug": report["slug"], "language": report["language"],
                "loc_six": report["loc"]["code_six"], "edges_total": report["edges"]}
        first = True
        for call, body in calls_for(verb, report):
            status, data, ms = client.post(verb, body)
            sink({**base, "call": call, "phase": "cold" if first else "first",
                  "ms": round(ms, 3), "body": body, **shape(verb, status, data)})
            first = False
            for rep in range(WARM_REPS):
                status, data, ms = client.post(verb, body)
                sink({**base, "call": call, "phase": "warm", "rep": rep, "ms": round(ms, 3),
                      **shape(verb, status, data)})
            if verb == "neighbors" and status == 200:
                sink({**base, "call": call, "phase": "fanout", **fanout(client, verb, body)})
        print(f"{verb}\t{report['slug']}\tdone", flush=True)
    if verb == "search":
        for word in ("error", "config", "parse"):
            for rep in range(WARM_REPS + 1):
                status, data, ms = client.post("search", {"query": word})
                sink({"pass": verb, "verb": "search", "slug": "*all*", "call": f"search.all.{word}",
                      "phase": "first" if rep == 0 else "warm", "ms": round(ms, 3),
                      **shape("search", status, data)})
    scrape_metrics(verb)


def main():
    verbs = sys.argv[1:] or ["node", "neighbors", "search", "history"]
    with open(os.path.join(common.RESULTS_DIR, "harness.jsonl")) as f:
        reports = [json.loads(line) for line in f if line.strip()]
    only = set(os.environ.get("BENCH_SLUGS", "").split())
    if only:
        reports = [r for r in reports if r["slug"] in only]
    out_path = os.path.join(common.RESULTS_DIR, "verb_calls.jsonl")
    with open(out_path, "a") as out:
        def sink(row):
            out.write(json.dumps(row, sort_keys=True) + "\n")
        for verb in verbs:
            run_pass(verb, reports, sink)
            out.flush()


if __name__ == "__main__":
    main()
