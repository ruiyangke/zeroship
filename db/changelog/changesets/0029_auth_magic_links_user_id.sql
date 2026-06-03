--liquibase formatted sql

-- Bind password-reset tokens to an IMMUTABLE user_id captured at issue time
-- (security finding L4).
--
-- `password_reset::complete` historically re-resolved its target with
-- `JOIN zeroship.users u ON u.email = ml.email`, so the account whose password
-- it set was whoever owned the email AT COMPLETE TIME — not the account the
-- reset was issued for. Today `email` is UNIQUE and no email-change /
-- account-recycle path exists, so the JOIN is 1:1; but the moment any future
-- email-mutation feature lands, an outstanding reset token silently retargets
-- to whichever account inherited the address (cross-account password set).
--
-- The OWASP Forgot-Password guidance is to bind a recovery token to an
-- immutable identifier. This adds a nullable `user_id` column to
-- `zeroship.magic_links`:
--   - `password_reset::issue` resolves the user once and stores its id here.
--   - `password_reset::complete` filters the candidate on this stored id;
--     email becomes display-only.
--
-- Nullable (not NOT NULL) because the magic-LINK login path also writes
-- `magic_links` rows and may issue a login token for an address with no user
-- yet (signup-via-link). Those rows leave `user_id` NULL; only reset rows
-- populate it. The `complete` candidate requires it IS NOT NULL.
--
-- ON DELETE CASCADE so deleting a user clears their outstanding reset rows.

--changeset zeroship:auth-magic-links-user-id splitStatements:true
ALTER TABLE zeroship.magic_links
    ADD COLUMN user_id UUID REFERENCES zeroship.users(id) ON DELETE CASCADE;
CREATE INDEX auth_magic_user_id_idx ON zeroship.magic_links (user_id);
--rollback DROP INDEX zeroship.auth_magic_user_id_idx;
--rollback ALTER TABLE zeroship.magic_links DROP COLUMN user_id;
