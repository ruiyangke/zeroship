--liquibase formatted sql

-- Backfill `zeroship.app_members(owner)` rows for apps created before
-- create_app started binding the creator as the app's owner (security finding
-- F3 / C1 over-restriction).
--
-- Authority over an app flows ONLY through an `app_members`-bound creator
-- policy (app_owner / app_editor / app_viewer). The C1 fix made the default
-- platform role a true zero-privilege role ("none"), so an app with NO owner
-- row leaves its creator unable to read/deploy/manage it. Going forward
-- `registry::create_app` writes the owner row atomically; this changeset repairs
-- pre-existing rows.
--
-- OWNER SOURCE. This schema has no `apps.creator_id` column — the only
-- data-level owner signal is `app_members` itself. We promote a member to
-- `owner` ONLY when ALL of the following hold:
--   1. the app currently has NO owner row (we never override an existing owner);
--   2. that member is the app's SOLE member (exactly one row); AND
--   3. the member is SELF-ORIGINATED — `added_by IS NULL` (legacy/pre-binding
--      rows have no adder) or `added_by = user_id` (they added themselves).
-- Clause 3 is the security guard. A sole member who was added BY a DIFFERENT
-- principal (`added_by <> user_id`) is a *delegated* editor/viewer, NOT the
-- app's creator: their adder is the rightful owner, so promoting the delegate
-- would be an over-grant. We therefore leave such delegated sole members at
-- their assigned role. (Today there is no editor/viewer member-add API, so this
-- only bites hand-seeded rows — but the guard keeps the backfill correct if one
-- is ever added, instead of silently escalating a viewer to owner.)
--
-- ON CONFLICT BEHAVIOR. The SELECT's principal is an EXISTING member row, so
-- the INSERT always collides on the `(app_id, user_id)` PK. We use
-- `DO UPDATE SET role = 'owner'` (NOT `DO NOTHING`): the row we want to promote
-- IS the conflicting row, so `DO NOTHING` would no-op the promotion and leave
-- the qualifying single-member app ownerless — defeating the whole changeset.
-- The clause-3 guard is what prevents that `DO UPDATE` from escalating a
-- delegated non-owner.
--
-- IDEMPOTENT: re-running is a no-op. After the first run the app has an owner
-- row, so clause 1 (`NOT EXISTS owner`) excludes it and the SELECT matches
-- nothing on subsequent runs.
--
-- PRE-LAUNCH NOTE: per AGENTS.md there are no production tenants, so in practice
-- this affects only dev/test databases. It is written to be correct regardless.

--changeset zeroship:app-members-owner-backfill splitStatements:true
--validCheckSum ANY
--   We mark this ANY because the backfill body was tightened (finding 2.0): the
--   SELECT now also requires the sole member to be SELF-ORIGINATED so a
--   delegated editor/viewer is never escalated to owner. That is a deliberate
--   logic change to an already-applied changeset; ANY lets an existing DB
--   re-migrate without a checksum failure. Re-running is still a no-op on a DB
--   that already has its owner rows (clause 1 excludes owned apps), and the
--   tightened predicate only ever REMOVES would-be promotions, so a DB that
--   ran the old body is not left in a worse state. Per the SQL-formatted
--   parser, this MUST be its own `--validCheckSum` line, NOT an inline
--   `--changeset` attribute (that form is silently dropped — see finding 6.0).
INSERT INTO zeroship.app_members (app_id, user_id, role)
SELECT m.app_id, m.user_id, 'owner'
FROM zeroship.app_members m
WHERE NOT EXISTS (
        SELECT 1 FROM zeroship.app_members o
        WHERE o.app_id = m.app_id AND o.role = 'owner'
    )
  AND (
        SELECT COUNT(*) FROM zeroship.app_members c WHERE c.app_id = m.app_id
    ) = 1
  AND (m.added_by IS NULL OR m.added_by = m.user_id)
ON CONFLICT (app_id, user_id) DO UPDATE SET role = 'owner';
--rollback /* ownership backfill is not reversible without the prior role snapshot */ SELECT 1;
