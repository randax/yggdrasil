-- Index jobs are queued when a fetch completes (0003 era onward), so a
-- repo whose last fetch finished before the indexing pipeline existed
-- would never be indexed. Queue the missing jobs once — only for repos
-- that have a synced commit and no Shard yet, so repos the pipeline
-- already covered aren't churned back through the queue.
--
-- FOR UPDATE + ORDER BY: take the repos row locks first, in a stable
-- order. This migration can run while an older replica's workers are
-- live, and every application transaction touching both tables locks
-- repos before jobs — inserting jobs rows first would invert that
-- (the FK's repos locks arrive at end of statement) and deadlock
-- against an in-flight completion.

INSERT INTO jobs (kind, repo_id)
SELECT 'index', id FROM repos
WHERE last_synced_commit IS NOT NULL AND current_shard_id IS NULL
ORDER BY id
FOR UPDATE
ON CONFLICT (repo_id, kind) WHERE state <> 'done' DO NOTHING;
