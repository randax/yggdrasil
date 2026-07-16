-- A discovered repository whose qualifier is already owned cannot be inserted
-- into repos. Record that rejected discovery beside the Forge org so one
-- collision does not wedge the rest of the listing and Admin can diagnose it.
CREATE TABLE forge_discovery_qualifier_conflicts (
    forge_org_id bigint NOT NULL REFERENCES forge_orgs (id) ON DELETE CASCADE,
    slug text NOT NULL,
    conflicting_repo_id bigint NOT NULL REFERENCES repos (id) ON DELETE CASCADE,
    first_seen_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (forge_org_id, slug)
);

CREATE INDEX forge_discovery_conflicts_by_repo
    ON forge_discovery_qualifier_conflicts (conflicting_repo_id);
