-- Control-plane repo registry: the slice of RFC 0001 §2's `repos` sketch
-- the server queries today. Forge linkage arrives with the forges table.
CREATE TABLE repos (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    slug text NOT NULL,
    visibility text,
    default_branch text,
    indexed boolean NOT NULL DEFAULT false,
    discovery_state text NOT NULL DEFAULT 'discovered',
    last_synced_commit text
);
