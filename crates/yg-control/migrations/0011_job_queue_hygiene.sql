-- Job queue hygiene and control-plane schema constraints (issue #49):
-- retention for terminal job rows, removal of the queue columns nothing
-- reads or writes, CHECK constraints mirroring the Rust vocabularies,
-- and the missing index behind the GC pointer anti-join.

-- payload was never read or written (a worker reads the repo's sync
-- position at claim time); priority was never set but drove an ORDER BY
-- the partial claim index (state, run_after) could not serve. The claim
-- query now orders by run_after alone.
ALTER TABLE jobs
    DROP COLUMN payload,
    DROP COLUMN priority;

-- Terminal rows accumulate forever without an anchor to age them by.
-- Every path that settles a job to 'done' stamps finished_at; rows
-- settled before this migration age from their creation instead.
ALTER TABLE jobs ADD COLUMN finished_at timestamptz;
UPDATE jobs SET finished_at = created_at WHERE state = 'done';
CREATE INDEX jobs_done_retention ON jobs (finished_at) WHERE state = 'done';

-- kind was free text: a typo'd row satisfies no claim query's kind
-- equality, so it sits invisible and unclaimable forever. The vocabulary
-- mirrors yg_control::JobKind; a new kind lands as a Rust variant plus a
-- migration extending this constraint.
ALTER TABLE jobs ADD CONSTRAINT jobs_kind_check
    CHECK (kind IN ('fetch', 'index'));

-- Same for the Shard provenance level, mirroring yg_shard::Provenance.
ALTER TABLE shards ADD CONSTRAINT shards_provenance_level_check
    CHECK (provenance_level IN ('precise', 'syntactic', 'extracted', 'inferred'));

-- The GC sweep's eligibility scan and delete guard anti-join every shard
-- row against the current-Shard pointer; without an index each probe
-- sequential-scans repos. Partial: NULL pointers can't match an equality
-- probe, and most repos have a current Shard.
CREATE INDEX repos_current_shard ON repos (current_shard_id)
    WHERE current_shard_id IS NOT NULL;

-- Re-derive every qualifier with the one grammar. 0006's backfill used a
-- scheme regex that diverged from Rust's repo_qualifier (a literal split
-- at the first "://"); this expression is asserted verbatim against
-- yg_control::REPO_QUALIFIER_SQL, whose tests pin it to the Rust
-- function. The unique index is rebuilt around the rewrite so
-- uniqueness is judged on the final state (a non-deferrable index
-- checks row-by-row mid-UPDATE and could trip on a transient swap); a
-- genuine collision under the corrected grammar still fails loudly on
-- the recreate, naming the duplicate — same stance as 0006.
DROP INDEX repos_qualifier;
UPDATE repos r
SET qualifier = rtrim(CASE WHEN strpos(f.base_url, '://') > 0
                           THEN substr(f.base_url, strpos(f.base_url, '://') + 3)
                           ELSE f.base_url END, '/') || '/' || r.slug
FROM forges f
WHERE f.id = r.forge_id;
CREATE UNIQUE INDEX repos_qualifier ON repos (qualifier);
