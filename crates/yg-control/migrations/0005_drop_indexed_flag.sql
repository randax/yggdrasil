-- repos.indexed (0001) duplicated what current_shard_id (0003) already
-- encodes: a repo is indexed exactly when it has a current Shard. Two
-- columns carrying one fact drift apart the first time a write path
-- updates one and not the other; the indexed-repo count now derives
-- from the pointer.

ALTER TABLE repos DROP COLUMN indexed;
