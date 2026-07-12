-- Explicit REST API root on the Forge record (issue #53). Discovery
-- reads this field instead of inferring the API root from the clone
-- root; registration computes the forge's default and stores it, and
-- tests point it at their fixture servers explicitly. NULL for forges
-- without an API (plain git remotes).

ALTER TABLE forges
    ADD COLUMN api_root text;

-- Backfill the forges that have an API today so discovery keeps running
-- across the upgrade without a manual re-add. The mapping mirrors the
-- GitHub adapter's default at the time of this migration (github.com's
-- API lives on its own host; GitHub Enterprise serves /api/v3 on the
-- clone host).
UPDATE forges
SET api_root = CASE
        WHEN rtrim(base_url, '/') = 'https://github.com'
            THEN 'https://api.github.com'
        ELSE rtrim(base_url, '/') || '/api/v3'
    END
WHERE kind = 'github' AND api_root IS NULL;
