-- Index jobs are queued when a fetch completes (0003 era onward), so a
-- repo whose last fetch finished before the indexing pipeline existed
-- would never be indexed. Queue the missing jobs once; the conflict
-- guard makes this a no-op wherever a job already exists.

INSERT INTO jobs (kind, repo_id)
SELECT 'index', id FROM repos WHERE last_synced_commit IS NOT NULL
ON CONFLICT (repo_id, kind) WHERE state <> 'done' DO NOTHING;
