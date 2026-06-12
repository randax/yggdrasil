-- repos.indexed (0001) duplicated what current_shard_id (0003) already
-- encodes: a repo is indexed exactly when it has a current Shard. Two
-- columns carrying one fact drift apart the first time a write path
-- updates one and not the other; the indexed-repo count now derives
-- from the pointer.
--
-- Dropping the column is a contraction: a binary from before this
-- migration still queries `WHERE indexed`, and its status endpoint
-- breaks the moment this runs. Acceptable while nothing old keeps
-- serving through a migration (pre-production); once deployments roll,
-- contractions ship one release after the last reader stops.

ALTER TABLE repos DROP COLUMN indexed;
