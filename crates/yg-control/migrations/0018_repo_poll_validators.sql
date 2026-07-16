-- HTTP validators for conditional repository-head polls. BYTEA preserves the
-- opaque header values exactly; NULL means the Forge has not supplied one.

ALTER TABLE repos
    ADD COLUMN poll_etag bytea,
    ADD COLUMN poll_last_modified bytea,
    ADD COLUMN poll_observed_head text;
