-- Control-plane repo registry (RFC 0001 §2). This slice only needs to
-- count indexed repos; the remaining §2 columns (forge linkage, slug,
-- visibility, default branch, discovery state, sync cursors) arrive
-- with the slices that use them.
CREATE TABLE repos (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    indexed boolean NOT NULL DEFAULT false
);
