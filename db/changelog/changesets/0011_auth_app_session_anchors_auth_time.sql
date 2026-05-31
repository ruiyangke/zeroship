--liquibase formatted sql

-- auth.app_session_anchors.auth_time — the unix instant of the authenticating
-- event (from the validated id_token at anchor-create). Read by the
-- server-side power-token mint (R4, 2026-05-30-console-as-regular-app §step-up /
-- 2026-05-30-auth-bff-session-redesign §5.3) to gate step-up scopes
-- (deploy / secrets / billing / delete) on a fresh re-auth. NULL → treated as
-- "no recent auth" → step-up scopes fail closed.
--
-- Append-only changeset (mirrors 0010 for auth.gateway_sessions): the anchor
-- table is created in 0006 and that changeset is immutable once applied, so the
-- column is added here rather than edited into 0006 (the includeAll convention
-- in db.changelog-master.yaml). IF NOT EXISTS keeps it idempotent against a dev
-- DB where the column may have been added out-of-band during R4 development.

--changeset zeroship:auth-app-session-anchors-auth-time splitStatements:true
ALTER TABLE auth.app_session_anchors ADD COLUMN IF NOT EXISTS auth_time TIMESTAMPTZ;
--rollback ALTER TABLE auth.app_session_anchors DROP COLUMN auth_time;
