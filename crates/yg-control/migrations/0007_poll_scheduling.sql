-- Continuous Sync poll scheduling (RFC 0001 §3, issue #9): each repo
-- carries when it is next due for a default-branch head check and its
-- own interval override; each forge carries the budget of conditional
-- requests the poll loop spends watching it.

ALTER TABLE repos
    -- Per-repo override; NULL falls back to the server's default interval.
    ADD COLUMN poll_interval_seconds integer CHECK (poll_interval_seconds >= 1),
    -- When this repo is next eligible for a poll. DEFAULT now() makes a
    -- freshly registered repo due the moment its first fetch lands (the
    -- claim gates on last_synced_commit), and stamps every repo that
    -- predates this migration as due immediately. The poll loop advances
    -- it by one jittered interval each time it claims the repo.
    ADD COLUMN next_poll_at timestamptz NOT NULL DEFAULT now();

-- The claim scan: the most-overdue synced repo. Only synced repos are
-- ever polled — there is nothing to detect a change against before the
-- first fetch — so the index need not carry the rest.
CREATE INDEX repos_poll_due ON repos (next_poll_at) WHERE last_synced_commit IS NOT NULL;

ALTER TABLE forges
    -- Conditional requests per minute the poll loop may spend against
    -- this forge (the RFC 0001 §2 rate_budget). A generous default for
    -- git ls-remote against an anonymous endpoint; lowered per forge when
    -- a token's budget is tighter.
    ADD COLUMN rate_budget integer NOT NULL DEFAULT 300 CHECK (rate_budget >= 1);
