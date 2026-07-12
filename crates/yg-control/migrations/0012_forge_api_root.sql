-- Explicit REST API root on the Forge record (issue #53). Discovery
-- reads this field instead of inferring the API root from the clone
-- root; registration computes the forge's default and stores it, and
-- tests point it at their fixture servers explicitly. NULL for forges
-- without an API (plain git remotes).

ALTER TABLE forges
    ADD COLUMN api_root text;
