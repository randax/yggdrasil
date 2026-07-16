-- Fleet-wide Forge request budgets. Every worker charging a Forge token
-- serializes through this row, so the configured burst and refill rate are
-- shared rather than multiplied by the number of worker processes.

CREATE TABLE forge_rate_budgets (
    forge_id bigint PRIMARY KEY REFERENCES forges(id) ON DELETE CASCADE,
    tokens double precision NOT NULL CHECK (tokens >= 0),
    refill_per_second double precision NOT NULL CHECK (refill_per_second > 0),
    updated_at timestamptz NOT NULL,
    cooldown_until timestamptz
);
