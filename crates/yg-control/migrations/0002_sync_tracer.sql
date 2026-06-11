-- Sync tracer slice (RFC 0001 §2–3): forge registry, repo registration
-- via include rules, and the Postgres job queue that feeds workers.

CREATE TABLE forges (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    -- github | git (generic). gitlab/codeberg arrive with their adapters.
    kind text NOT NULL,
    base_url text NOT NULL UNIQUE,
    -- Name of the environment variable holding this Forge's token
    -- (token_ref in the RFC sketch); the token itself is never stored.
    token_env text,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- 0001 shipped repos(id, indexed) only; the sync columns arrive here.
-- The table has never held production rows, so NOT NULL without
-- defaults is safe.
ALTER TABLE repos
    ADD COLUMN forge_id bigint NOT NULL REFERENCES forges (id),
    ADD COLUMN slug text NOT NULL,
    ADD COLUMN discovery_state text NOT NULL DEFAULT 'included'
        CHECK (discovery_state IN ('discovered', 'included', 'excluded')),
    ADD COLUMN last_synced_commit text,
    -- NULL = full history (the default); set = shallow --depth override.
    ADD COLUMN fetch_depth integer CHECK (fetch_depth >= 1),
    ADD COLUMN created_at timestamptz NOT NULL DEFAULT now();
CREATE UNIQUE INDEX repos_forge_slug ON repos (forge_id, slug);

CREATE TABLE rules (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    forge_id bigint NOT NULL REFERENCES forges (id),
    pattern text NOT NULL,
    action text NOT NULL CHECK (action IN ('include', 'exclude')),
    applies_to_private boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (forge_id, pattern, action)
);

-- Job queue: claimed with FOR UPDATE SKIP LOCKED under a lease; a crashed
-- worker's job becomes claimable again when its lease expires.
CREATE TABLE jobs (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    -- fetch (index/embed kinds arrive with their milestones)
    kind text NOT NULL,
    repo_id bigint NOT NULL REFERENCES repos (id),
    payload jsonb NOT NULL DEFAULT '{}'::jsonb,
    state text NOT NULL DEFAULT 'queued'
        CHECK (state IN ('queued', 'leased', 'done')),
    priority integer NOT NULL DEFAULT 0,
    attempts integer NOT NULL DEFAULT 0,
    last_error text,
    run_after timestamptz NOT NULL DEFAULT now(),
    lease_until timestamptz,
    created_at timestamptz NOT NULL DEFAULT now()
);
-- Repeated `repo add` must not pile up duplicate work: at most one
-- in-flight job per (repo, kind).
CREATE UNIQUE INDEX jobs_one_in_flight_per_repo_kind
    ON jobs (repo_id, kind) WHERE state <> 'done';
CREATE INDEX jobs_claim_scan ON jobs (state, run_after) WHERE state <> 'done';
