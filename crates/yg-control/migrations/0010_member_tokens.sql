-- Member bearer tokens for humans and agents. Token material is shown
-- once at issue time; the database keeps only a hash plus lifecycle
-- timestamps.

CREATE TABLE member_tokens (
    id text PRIMARY KEY,
    member text NOT NULL,
    token_hash text NOT NULL UNIQUE,
    created_at timestamptz NOT NULL DEFAULT now(),
    last_used_at timestamptz,
    revoked_at timestamptz
);

CREATE INDEX member_tokens_active_hash
    ON member_tokens (token_hash)
    WHERE revoked_at IS NULL;
