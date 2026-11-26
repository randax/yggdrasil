"""End-to-end indexing on the real stack, one repo at a time.

For each corpus repo: `yg admin repo add`, then poll Postgres until the repo's
current Shard is published (or the per-repo timeout passes). Repos are added
strictly sequentially so the fetch and index of one repo never overlap
another's; the timings are therefore free of queue wait. Resumable: repos
that already have a current Shard are re-reported, not re-added.

Output: results/index_e2e.tsv
Usage: python3 -I index_e2e.py [slug ...]
"""

import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import common  # noqa: E402

TIMEOUT_S = 20 * 60
FIELDS = [
    "slug", "language", "repo_id", "commit_sha", "nodes", "edges", "status",
    "wall_add_to_published_s", "fetch_job_s", "fetch_done_to_published_s",
    "index_job_s", "repo_created_epoch", "fetch_created_epoch", "fetch_finished_epoch",
    "index_created_epoch", "index_finished_epoch", "published_epoch", "mirror_bytes",
    "last_error",
]


def repo_state(slug):
    rows = common.psql(
        "select r.id, coalesce(s.commit_sha,''), coalesce(s.node_count::text,''),"
        " coalesce(s.edge_count::text,''), extract(epoch from r.created_at),"
        " coalesce(extract(epoch from s.published_at)::text,'')"
        f" from repos r left join shards s on s.id = r.current_shard_id where r.slug = '{slug}'"
    )
    return rows[0] if rows else None


def job_times(repo_id, kind):
    """(created, finished, last_error) of the latest job of `kind`."""
    rows = common.psql(
        "select extract(epoch from created_at), coalesce(extract(epoch from finished_at)::text,''),"
        f" coalesce(replace(last_error, E'\\n', ' '),'') from jobs where repo_id = {repo_id}"
        f" and kind = '{kind}' order by id desc limit 1"
    )
    return rows[0] if rows else ["", "", ""]


def du_bytes(path):
    if not os.path.exists(path):
        return ""
    out = subprocess.run(["du", "-sk", path], capture_output=True, text=True).stdout
    return str(int(out.split()[0]) * 1024)


def f(x):
    return float(x) if x not in ("", None) else None


def diff(a, b):
    a, b = f(a), f(b)
    return f"{b - a:.3f}" if a is not None and b is not None else ""


def index_one(slug, language):
    state = repo_state(slug)
    wall = ""
    status = "existing"
    if state is None or not state[1]:
        start = time.monotonic()
        if state is None:
            subprocess.run(
                [os.environ["YG_BIN"], "admin", "repo", "add", f"https://github.com/{slug}", "--json"],
                check=True, capture_output=True,
            )
        status = "timeout"
        while time.monotonic() - start < TIMEOUT_S:
            state = repo_state(slug)
            if state and state[1]:
                status = "ok"
                break
            time.sleep(0.5)
        wall = f"{time.monotonic() - start:.3f}"
        state = repo_state(slug)
    repo_id, commit, nodes, edges, created, published = state
    fetch = job_times(repo_id, "fetch")
    index = job_times(repo_id, "index")
    return {
        "slug": slug, "language": language, "repo_id": repo_id, "commit_sha": commit,
        "nodes": nodes, "edges": edges, "status": status,
        "wall_add_to_published_s": wall,
        "fetch_job_s": diff(fetch[0], fetch[1]),
        "fetch_done_to_published_s": diff(fetch[1], published),
        "index_job_s": diff(index[0], index[1]),
        "repo_created_epoch": created, "fetch_created_epoch": fetch[0],
        "fetch_finished_epoch": fetch[1], "index_created_epoch": index[0],
        "index_finished_epoch": index[1], "published_epoch": published,
        "mirror_bytes": du_bytes(os.path.join(os.environ["YG_GIT_CACHE"], f"{repo_id}.git")),
        "last_error": fetch[2] or index[2],
    }


def main():
    wanted = set(sys.argv[1:])
    out_path = os.path.join(common.RESULTS_DIR, "index_e2e.tsv")
    done = {r["slug"]: r for r in common.read_tsv(out_path)} if os.path.exists(out_path) else {}
    for entry in common.corpus():
        slug = entry["slug"]
        if wanted and slug not in wanted:
            continue
        if slug in done and done[slug]["status"] == "ok":
            continue
        row = index_one(slug, entry["language"])
        done[slug] = row
        print(f"{slug}\t{row['status']}\twall={row['wall_add_to_published_s']}"
              f"\tfetch={row['fetch_job_s']}\tindex={row['index_job_s']}"
              f"\tnodes={row['nodes']}\tedges={row['edges']}", flush=True)
        order = [e["slug"] for e in common.corpus() if e["slug"] in done]
        common.write_tsv(out_path, [done[s] for s in order], FIELDS)


if __name__ == "__main__":
    main()
