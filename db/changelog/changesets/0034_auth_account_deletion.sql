--liquibase formatted sql

-- Account-deletion / GDPR-erase lifecycle columns (ISS-12).
--
-- GDPR Art. 17 requires a self-service erase path. The lifecycle is:
--
--   1. The user POSTs /me/delete. We IMMEDIATELY soft-disable the account
--      (`disabled_at = NOW()`, the existing login/eligibility gate already
--      rejects a disabled row), record `deletion_requested_at = NOW()`, and
--      schedule erasure for `deletion_scheduled_for = NOW() + 30 days`. A
--      confirm/undo email goes out and the active sessions are revoked.
--   2. Within the grace window the user can POST /me/delete/cancel, which
--      clears all three timestamps AND `disabled_at`, fully reactivating the
--      account.
--   3. Once `deletion_scheduled_for <= NOW()` and `anonymized_at IS NULL`, the
--      account reaper (crates/auth/src/cron/account_reaper.rs) erases the
--      account: either a hard DELETE (the 10 ON DELETE CASCADE FKs tear down
--      dependents) for a user with no financial history, or — for a creator
--      with Stripe-Connect / billing rows we are legally obliged to retain
--      (Art. 17(3)(b)) — an in-place PII ANONYMIZE that stamps `anonymized_at`
--      and leaves the financial rows intact. `anonymized_at` is the terminal,
--      idempotent marker that makes the reaper skip an already-erased row.
--
-- All three are nullable: NULL on every existing/new row means "no deletion in
-- flight", which is the correct default. No backfill (pre-launch, no rows).

--changeset zeroship:auth-users-account-deletion splitStatements:true
ALTER TABLE zeroship.users
    ADD COLUMN deletion_requested_at  TIMESTAMPTZ,
    ADD COLUMN deletion_scheduled_for TIMESTAMPTZ,
    ADD COLUMN anonymized_at          TIMESTAMPTZ;
-- Partial index backing the reaper's due-scan: only rows with a pending,
-- not-yet-anonymized erasure are ever selected, so the index stays tiny.
CREATE INDEX auth_users_deletion_due_idx
    ON zeroship.users (deletion_scheduled_for)
    WHERE deletion_scheduled_for IS NOT NULL AND anonymized_at IS NULL;
--rollback DROP INDEX zeroship.auth_users_deletion_due_idx;
--rollback ALTER TABLE zeroship.users DROP COLUMN anonymized_at;
--rollback ALTER TABLE zeroship.users DROP COLUMN deletion_scheduled_for;
--rollback ALTER TABLE zeroship.users DROP COLUMN deletion_requested_at;
