-- Preserve the normalized Forge root spelling used for the first repo
-- registration. Forge classification may select a differently spelled
-- canonical Forge row, but later adds still need to distinguish an idempotent
-- re-add from a second-scheme qualifier conflict.
ALTER TABLE repos ADD COLUMN registration_base_url text;

-- Existing rows predate spelling provenance, so their canonical Forge root is
-- the only spelling known. Keep the column nullable for external schema
-- fixtures that insert legacy-shaped rows; all application insert paths write
-- it explicitly.
UPDATE repos r
SET registration_base_url = f.base_url
FROM forges f
WHERE f.id = r.forge_id;
