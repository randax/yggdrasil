-- The repo qualifier (RFC 0001 §5): the Forge root sans scheme joined
-- with the slug — the repo part of every external node id, and what
-- Verb queries resolve. Owned by Rust (`repo_qualifier`, computed at
-- registration); this backfill mirrors it for rows registered before
-- the column existed (pre-release dev data only).
ALTER TABLE repos ADD COLUMN qualifier text;

-- Case-insensitive scheme match, like Rust's split at "://".
UPDATE repos r
SET qualifier = rtrim(regexp_replace(f.base_url, '^[a-zA-Z][a-zA-Z0-9+.-]*://', ''), '/')
                || '/' || r.slug
FROM forges f
WHERE f.id = r.forge_id;

ALTER TABLE repos ALTER COLUMN qualifier SET NOT NULL;

-- One repo per qualifier: external ids must resolve unambiguously, so
-- the same host/slug reached via two schemes (or otherwise-colliding
-- forge roots) is rejected at registration instead of answering
-- queries nondeterministically. If a pre-release database already
-- holds such a collision, this index creation fails loudly naming the
-- duplicate — delete the unwanted repos row and re-run.
CREATE UNIQUE INDEX repos_qualifier ON repos (qualifier);
