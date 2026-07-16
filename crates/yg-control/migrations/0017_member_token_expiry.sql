-- Member tokens may expire at an administrator-selected instant. NULL keeps
-- the existing non-expiring behavior for tokens issued without a lifetime.

ALTER TABLE member_tokens
    ADD COLUMN expires_at timestamptz;
