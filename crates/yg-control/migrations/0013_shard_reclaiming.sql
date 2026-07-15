-- Torn-Shard repair (issue #40): GC first claims a Shard row instead of
-- deleting the uniqueness fence before object cleanup. A publisher that
-- collides with reclaiming requeues; the GC worker reaps the row only
-- after manifest-first object deletion has completed.
ALTER TABLE shards ADD COLUMN state text NOT NULL DEFAULT 'published';
ALTER TABLE shards ADD CONSTRAINT shards_state_check
    CHECK (state IN ('published', 'reclaiming'));
