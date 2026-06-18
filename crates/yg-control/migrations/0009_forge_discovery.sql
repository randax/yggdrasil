-- Forge org discovery and private opt-in rules (RFC 0001 §2, ADR 0001,
-- issue #10). Existing manually-added repos are public/included by
-- default, preserving their current sync behavior.

ALTER TABLE repos
    ADD COLUMN visibility text NOT NULL DEFAULT 'public'
        CHECK (visibility IN ('public', 'internal', 'private'));

CREATE TABLE forge_orgs (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    forge_id bigint NOT NULL REFERENCES forges (id),
    org_slug text NOT NULL,
    token_env text,
    next_discovery_at timestamptz NOT NULL DEFAULT now(),
    last_discovered_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (forge_id, org_slug)
);

CREATE INDEX forge_orgs_discovery_due ON forge_orgs (next_discovery_at);
