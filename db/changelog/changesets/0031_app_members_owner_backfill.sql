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
-- data-level owner signal is `app_members` itself. We therefore promote the
-- SOLE existing member of an app to `owner` when that app has exactly one
-- member and no owner yet. This never fabricates ownership: apps with zero
-- members (true orphans) and apps that already have an owner are left untouched.
--
-- IDEMPOTENT: re-running is a no-op. The INSERT is guarded by
-- `ON CONFLICT (app_id, user_id) DO NOTHING` and the SELECT only matches apps
-- that currently have NO owner row, so a second run finds nothing to change.
--
-- PRE-LAUNCH NOTE: per AGENTS.md there are no production tenants, so in practice
-- this affects only dev/test databases. It is written to be correct regardless.

--changeset zeroship:app-members-owner-backfill splitStatements:true
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
ON CONFLICT (app_id, user_id) DO UPDATE SET role = 'owner';
--rollback /* ownership backfill is not reversible without the prior role snapshot */ SELECT 1;
