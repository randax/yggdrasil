-- Index jobs are queued when a fetch completes (0003 era onward), so a
-- repo whose last fetch finished before the indexing pipeline existed
-- would never be indexed. Queue the missing jobs once — only for repos
-- that have a synced commit and no Shard yet, so repos the pipeline
-- already covered aren't churned back through the queue.

INSERT INTO jobs (kind, repo_id)
SELECT 'index', id FROM repos
WHERE last_synced_commit IS NOT NULL AND current_shard_id IS NULL
ON CONFLICT (repo_id, kind) WHERE state <> 'done' DO NOTHING;
