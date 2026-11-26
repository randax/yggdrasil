"""Per-pass timing over the server's own bare mirrors, plus LOC and hub picks.

For each repo with a published Shard (results/index_e2e.tsv): run the scratch
harness binary under `/usr/bin/time -l` (peak RSS) on the same mirror and
commit the server indexed, count LOC in the extracted tree with tokei, pick
Verb arguments (hubs.py) from the local graph segment, keep that segment
under $STATE_DIR/graphs/<repo_id>.sqlite only for the KEEP_GRAPHS repos
(dbstat breakdown), and delete the rest of the scratch output (disk).

Outputs: results/harness.jsonl (one object per repo), corpus.tsv
Usage: python3 -I harness.py [slug ...]
"""

import json
import os
import re
import shutil
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import common  # noqa: E402
import hubs  # noqa: E402

KEEP_GRAPHS = set(os.environ.get(
    "KEEP_GRAPHS", "gin-gonic/gin etcd-io/etcd django/django pydantic/pydantic").split())

SIX = {
    "Go": "Go", "Java": "Java", "JavaScript": "JavaScript", "JSX": "JavaScript",
    "TypeScript": "TypeScript", "TSX": "TypeScript", "Python": "Python", "Rust": "Rust",
}


def loc(tree):
    out = subprocess.run(
        ["tokei", "--hidden", "--no-ignore", "-o", "json", tree],
        check=True, capture_output=True, text=True,
    ).stdout
    data = json.loads(out)
    per = {lang: 0 for lang in sorted(set(SIX.values()))}
    total = 0
    for lang, stats in data.items():
        if lang == "Total":
            continue
        code = stats.get("code", 0)
        total += code
        if lang in SIX:
            per[SIX[lang]] += code
    return {"code_all_languages": total, "code_six": sum(per.values()), "per_language": per}


def run_harness(mirror, commit, out_dir):
    proc = subprocess.run(
        ["/usr/bin/time", "-l", os.environ["HARNESS_BIN"], mirror, commit, out_dir],
        capture_output=True, text=True,
    )
    if proc.returncode != 0:
        raise RuntimeError(proc.stderr[-2000:])
    report = json.loads(proc.stdout.strip().splitlines()[-1])
    rss = re.search(r"(\d+)\s+maximum resident set size", proc.stderr)
    real = re.search(r"([\d.]+) real", proc.stderr)
    report["max_rss_bytes"] = int(rss.group(1)) if rss else None
    report["process_real_s"] = float(real.group(1)) if real else None
    return report


def tree_bytes(tree):
    total = 0
    files = 0
    for root, _, names in os.walk(tree):
        for name in names:
            path = os.path.join(root, name)
            if not os.path.islink(path):
                total += os.path.getsize(path)
                files += 1
    return total, files


def main():
    wanted = set(sys.argv[1:])
    e2e = [r for r in common.read_tsv(os.path.join(common.RESULTS_DIR, "index_e2e.tsv"))
           if r["status"] in ("ok", "existing") and r["commit_sha"]]
    out_path = os.path.join(common.RESULTS_DIR, "harness.jsonl")
    done = {}
    if os.path.exists(out_path):
        with open(out_path) as f:
            done = {json.loads(line)["slug"]: json.loads(line) for line in f if line.strip()}
    graphs = os.path.join(common.STATE_DIR, "graphs")
    os.makedirs(graphs, exist_ok=True)
    for row in e2e:
        slug = row["slug"]
        if (wanted and slug not in wanted) or (not wanted and slug in done):
            continue
        mirror = os.path.join(os.environ["YG_GIT_CACHE"], f"{row['repo_id']}.git")
        out_dir = os.path.join(common.STATE_DIR, "harness-scratch", row["repo_id"])
        shutil.rmtree(out_dir, ignore_errors=True)
        report = run_harness(mirror, row["commit_sha"], out_dir)
        tree = os.path.join(out_dir, "tree")
        report["tree_bytes"], report["tree_files"] = tree_bytes(tree)
        report["loc"] = loc(tree)
        report.update(slug=slug, language=row["language"], repo_id=row["repo_id"])
        seg_dir = report["segments"]["dir"]
        report["hubs"] = hubs.pick(os.path.join(seg_dir, "graph.sqlite"))
        if slug in KEEP_GRAPHS:
            shutil.copy(os.path.join(seg_dir, "graph.sqlite"),
                        os.path.join(graphs, f"{row['repo_id']}.sqlite"))
        shutil.rmtree(out_dir, ignore_errors=True)
        done[slug] = report
        t = report["timings_s"]
        print(f"{slug}\tloc={report['loc']['code_six']}\tsyntactic={t['syntactic_pass']:.2f}"
              f"\thistory={t['history_walk']:.2f}\tprepare={t['prepare_shard']:.2f}"
              f"\trss={report['max_rss_bytes']}", flush=True)
        order = [r["slug"] for r in e2e if r["slug"] in done]
        with open(out_path, "w") as f:
            for s in order:
                f.write(json.dumps(done[s], sort_keys=True) + "\n")
    write_corpus(e2e, done)


def write_corpus(e2e, done):
    rows = []
    for r in e2e:
        h = done.get(r["slug"])
        if not h:
            continue
        per = h["loc"]["per_language"]
        rows.append({
            "slug": r["slug"], "commit_sha": r["commit_sha"], "primary_language": r["language"],
            "loc_primary": per[r["language"]], "loc_six_languages": h["loc"]["code_six"],
            "loc_all_languages": h["loc"]["code_all_languages"], "files": h["tree_files"],
            "tree_bytes": h["tree_bytes"],
            **{f"loc_{k.lower()}": v for k, v in per.items()},
        })
    fields = ["slug", "commit_sha", "primary_language", "loc_primary", "loc_six_languages",
              "loc_all_languages", "files", "tree_bytes"] + [
        f"loc_{k.lower()}" for k in sorted(set(SIX.values()))]
    common.write_tsv(os.path.join(common.PROFILE_DIR, "corpus.tsv"), rows, fields)


if __name__ == "__main__":
    main()
