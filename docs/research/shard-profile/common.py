"""Shared helpers for the shard-profile scripts (stdlib only; run with python3 -I).

Every script reads its settings from the environment exported by env.sh.
"""

import csv
import json
import os
import subprocess
import urllib.request

ENV = os.environ
PROFILE_DIR = ENV["PROFILE_DIR"]
RESULTS_DIR = ENV["RESULTS_DIR"]
STATE_DIR = ENV["STATE_DIR"]
SERVER = ENV["YG_SERVER"]
TOKEN = ENV["YG_TOKEN"]
PG_CONTAINER = ENV["COMPOSE_PROJECT"] + "-postgres-1"


def psql(sql):
    """Run one query in the stack's Postgres; return rows as lists of str."""
    out = subprocess.run(
        ["docker", "exec", PG_CONTAINER, "psql", "-U", "yggdrasil", "-At", "-F", "\t", "-c", sql],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return [line.split("\t") for line in out.splitlines() if line]


def s3_get(key, query=""):
    """GET an object (or bucket listing) from RustFS with SigV4 via curl."""
    url = f"{ENV['YG_S3_ENDPOINT']}/{ENV['YG_S3_BUCKET']}/{key}{query}"
    return subprocess.run(
        ["curl", "-fsS", "--aws-sigv4", "aws:amz:us-east-1:s3", "--user", "yggdrasil:yggdrasil", url],
        check=True,
        capture_output=True,
    ).stdout


def read_tsv(path):
    with open(path, newline="") as f:
        return list(csv.DictReader(f, delimiter="\t"))


def write_tsv(path, rows, fields):
    with open(path, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=fields, delimiter="\t", extrasaction="ignore", lineterminator="\n")
        w.writeheader()
        for row in rows:
            w.writerow(row)


def corpus():
    return read_tsv(os.path.join(PROFILE_DIR, "corpus.in.tsv"))


def post_verb(path, body, timeout=60):
    """POST a Verb; return (status, response bytes)."""
    req = urllib.request.Request(
        f"{SERVER}{path}",
        data=json.dumps(body).encode(),
        headers={"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, resp.read()
    except urllib.error.HTTPError as err:
        return err.code, err.read()


def percentile(values, p):
    if not values:
        return None
    ordered = sorted(values)
    k = (len(ordered) - 1) * p / 100
    lo, hi = int(k), min(int(k) + 1, len(ordered) - 1)
    return ordered[lo] + (ordered[hi] - ordered[lo]) * (k - lo)
