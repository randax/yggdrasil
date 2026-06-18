-- Shard garbage collection (issue #9): when the current-Shard pointer
-- swaps away from a Shard, stamp when it stopped being current. The GC
-- sweep reclaims a superseded Shard's object-storage segments only after
-- a grace window measured from this stamp, so a query that resolved the
-- old pointer just before the swap still finds its Shard while it reads.
--
-- NULL means the Shard is either the current pointer or was published
-- but never became current (a superseded index result that lost the swap
-- race); the sweep anchors those on published_at instead.
ALTER TABLE shards ADD COLUMN superseded_at timestamptz;
