--liquibase formatted sql

-- Per-user account lockout counter (security finding L5).
--
-- `zeroship.users.locked_until` was READ by the credential / eligibility
-- paths (identity/credentials.rs, identity/eligibility.rs, ui/link.rs) but
-- only ever SET by a test fixture — so progressive lockout was dead and
-- leaky-bucket rate limiting was the sole online-guessing defense.
--
-- This adds the durable counter the lockout logic needs:
--   - `record_login_failure` (store::users) bumps `failed_login_count` on each
--     wrong-password attempt and, once it reaches the threshold, stamps
--     `locked_until` with exponential backoff.
--   - `reset_login_failures` zeroes the counter and clears `locked_until` on a
--     successful login.
--
-- NOT NULL DEFAULT 0 so every existing/new row starts un-failed.

--changeset zeroship:auth-users-failed-login-count splitStatements:true
ALTER TABLE zeroship.users
    ADD COLUMN failed_login_count INTEGER NOT NULL DEFAULT 0;
--rollback ALTER TABLE zeroship.users DROP COLUMN failed_login_count;
