"""Dissect one example response per Verb: where do the payload bytes go?

Replays one call per Verb (from verbs_bench.py's argument picks) against the
running server and splits the response bytes into: node-id strings, the
repo qualifier repeated inside them, other string values, JSON keys, and
structural punctuation/numbers. Approximate tokens are bytes/4.

Output: results/anatomy_examples.json
Usage: ANATOMY_SLUG=etcd-io/etcd python3 -I anatomy.py   (server must be up)
"""

import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import common  # noqa: E402
import verbs_bench  # noqa: E402

CALLS = {"node": "node.top_symbol", "neighbors": "neighbors.top_any.d1",
         "search": "search.common_name", "history": "history.hot_file"}


def walk(value, qualifier, acc):
    if isinstance(value, dict):
        for k, v in value.items():
            acc["json_keys"] += len(k) + 3  # quotes and colon
            walk(v, qualifier, acc)
    elif isinstance(value, list):
        for v in value:
            walk(v, qualifier, acc)
    elif isinstance(value, str):
        n = len(json.dumps(value).encode())
        if f":{qualifier}:" in value:
            acc["node_id_strings"] += n
            acc["of_which_repo_qualifier"] += value.count(qualifier) * len(qualifier)
        elif value.startswith("v1.") and len(value) > 40:
            acc["cursor"] += n
        else:
            acc["other_strings"] += n


def dissect(data, qualifier):
    acc = dict.fromkeys(["node_id_strings", "of_which_repo_qualifier", "other_strings",
                         "cursor", "json_keys"], 0)
    walk(json.loads(data), qualifier, acc)
    total = len(data)
    acc["punctuation_numbers_bools"] = total - acc["node_id_strings"] - acc["other_strings"] \
        - acc["cursor"] - acc["json_keys"]
    return {"bytes": total, "approx_tokens": total // 4,
            "share": {k: round(v / total, 3) for k, v in acc.items()}, "bytes_by_part": acc}


def main():
    slug = os.environ.get("ANATOMY_SLUG", "etcd-io/etcd")
    with open(os.path.join(common.RESULTS_DIR, "harness.jsonl")) as f:
        report = next(json.loads(line) for line in f if json.loads(line)["slug"] == slug)
    qualifier = f"{verbs_bench.FORGE_HOST}/{slug}"
    out = {"slug": slug}
    for verb, call in CALLS.items():
        body = dict(verbs_bench.calls_for(verb, report))[call]
        status, data = common.post_verb(f"/v1/verbs/{verb}", body)
        if status == 200:
            parsed = json.loads(data)
            out[call] = {"body": body, **dissect(data, qualifier),
                         "top_level_bytes": {k: len(json.dumps(v)) for k, v in parsed.items()}}
        else:
            out[call] = {"body": body, "status": status}
    with open(os.path.join(common.RESULTS_DIR, "anatomy_examples.json"), "w") as f:
        json.dump(out, f, indent=1, sort_keys=True)
    print(json.dumps(out, indent=1, sort_keys=True))


if __name__ == "__main__":
    main()
