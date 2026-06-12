-- Shard registry (RFC 0001 §2, §6): every Shard revision a repo has
-- published, plus the repo's current-Shard pointer, swapped atomically
-- when an index job completes.

CREATE TABLE shards (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    repo_id bigint NOT NULL REFERENCES repos (id),
    revision text NOT NULL,
    -- Object-storage key of the Shard's manifest.json.
    manifest_key text NOT NULL,
    commit_sha text NOT NULL,
    -- syntactic now; precise arrives with the M1 SCIP pass (ADR 0002).
    provenance_level text NOT NULL,
    -- Cached from the manifest so status never reads object storage.
    node_count bigint NOT NULL,
    edge_count bigint NOT NULL,
    published_at timestamptz NOT NULL DEFAULT now(),
    -- Revisions are immutable: one row per (repo, revision), forever.
    UNIQUE (repo_id, revision)
);

ALTER TABLE repos ADD COLUMN current_shard_id bigint REFERENCES shards (id);
