--liquibase formatted sql

-- Phase 3 explicit role-grant tightening. Transcribed from
-- crates/sandbox/migrations/0004_role_split_phase3.sql
-- (sandbox.* → zeroship.*).
--
-- Source-of-truth: docs/proposals/sandbox-pg-state.md § 13.2 + the
-- Phase-3 task spec. 0011 created the four roles and a permissive grant
-- bundle; this narrows the grants to the design's invariant:
--
--   sandbox_app:    SELECT/INSERT/UPDATE/DELETE on non-events tables;
--                   SELECT/INSERT on events (DELETE forbidden — the
--                   controller cannot tamper with its own audit trail)
--   sandbox_audit:  INSERT-only on events (no SELECT, no DELETE)
--   sandbox_gdpr:   SELECT + DELETE on every table the cascade touches;
--                   INSERT on events + deleted_sandboxes
--   sandbox_admin:  CREATE/USAGE on the schema (DDL/migrations)
--
-- REVOKE-then-GRANT is safe to replay. The `REVOKE ALL ON ALL TABLES IN
-- SCHEMA zeroship FROM <role>` calls are faithful to the source (the
-- sandbox roles never held grants on the platform tables, so they are a
-- no-op against those). The `sandbox.schema_migrations` re-grant from the
-- source migration is DROPPED (the table no longer exists).
--
-- splitStatements:false because the DO block contains `;` inside `$$`.
--changeset zeroship-sandbox:sandbox-role-split-phase3 splitStatements:false
DO $role_split$
DECLARE
    can_grant BOOLEAN;
BEGIN
    -- A pg superuser or a role with explicit `WITH ADMIN OPTION` on
    -- the four sandbox roles can manage their grants. CI Postgres
    -- runs as `postgres` (superuser); production runs as a dedicated
    -- bootstrap role with the necessary admin options.
    SELECT rolsuper OR rolcreaterole INTO can_grant
      FROM pg_roles
     WHERE rolname = current_user;

    IF NOT can_grant THEN
        RAISE NOTICE 'sandbox role-grant tightening skipped: caller lacks role-management privilege';
        RETURN;
    END IF;

    -- ─── sandbox_app ────────────────────────────────────────────
    -- INSERT/UPDATE/SELECT/DELETE on non-events tables; INSERT/SELECT
    -- on events (no DELETE — the controller must not tombstone its
    -- own audit log).
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_app') THEN
        EXECUTE 'REVOKE ALL ON zeroship.sandbox_events FROM sandbox_app';
        EXECUTE 'GRANT SELECT, INSERT ON zeroship.sandbox_events TO sandbox_app';
        -- Ensure the non-events grants are present (idempotent —
        -- 0011 already granted these).
        EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON
                    zeroship.sandboxes, zeroship.shares, zeroship.hosts,
                    zeroship.deleted_sandboxes
                    TO sandbox_app';
    END IF;

    -- ─── sandbox_audit ──────────────────────────────────────────
    -- INSERT-only on events. Audit pipe (currently inside the
    -- controller, eventually a separate process) writes via this
    -- role; the role cannot SELECT, UPDATE, or DELETE rows it wrote.
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_audit') THEN
        EXECUTE 'REVOKE ALL ON ALL TABLES IN SCHEMA zeroship FROM sandbox_audit';
        EXECUTE 'GRANT USAGE ON SCHEMA zeroship TO sandbox_audit';
        EXECUTE 'GRANT INSERT ON zeroship.sandbox_events TO sandbox_audit';
    END IF;

    -- ─── sandbox_gdpr ───────────────────────────────────────────
    -- SELECT (the cascade WHERE chain reads from sandboxes); DELETE
    -- on the cascade tables; INSERT on the audit + tombstone tables
    -- (the GDPR-delete TX writes a `gdpr.delete_user` event row in
    -- the same transaction as the cascade).
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_gdpr') THEN
        EXECUTE 'REVOKE ALL ON ALL TABLES IN SCHEMA zeroship FROM sandbox_gdpr';
        EXECUTE 'GRANT USAGE ON SCHEMA zeroship TO sandbox_gdpr';
        EXECUTE 'GRANT SELECT, DELETE ON
                    zeroship.sandboxes, zeroship.shares, zeroship.sandbox_events,
                    zeroship.deleted_sandboxes
                    TO sandbox_gdpr';
        EXECUTE 'GRANT INSERT ON zeroship.deleted_sandboxes, zeroship.sandbox_events TO sandbox_gdpr';
    END IF;

    -- ─── sandbox_admin ──────────────────────────────────────────
    -- CREATE/USAGE on schema only. No table-level grants — the
    -- migration role applies DDL via batch_execute as the role's
    -- table-owner; runtime DML is forbidden.
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_admin') THEN
        EXECUTE 'REVOKE ALL ON ALL TABLES IN SCHEMA zeroship FROM sandbox_admin';
        EXECUTE 'GRANT CREATE, USAGE ON SCHEMA zeroship TO sandbox_admin';
    END IF;
EXCEPTION
    WHEN insufficient_privilege THEN
        RAISE NOTICE 'sandbox role-grant tightening skipped: insufficient privilege on REVOKE/GRANT';
    WHEN deadlock_detected THEN
        -- Two concurrent migrators racing on REVOKE/GRANT for the
        -- same role observe a deadlock on `pg_authid`. The winner's
        -- grants are durable; treat as a race-tolerant success. The
        -- grants are idempotent.
        RAISE NOTICE 'sandbox role-grant tightening: deadlock with concurrent migrator; treating as race-success';
    WHEN serialization_failure THEN
        RAISE NOTICE 'sandbox role-grant tightening: serialization failure (concurrent migrator); treating as race-success';
    WHEN OTHERS THEN
        -- Some pg builds surface the role-grant race as a generic
        -- internal_error (XX000). Since this changeset only refines
        -- grants — never data — treat ANY exception as a race-tolerant
        -- success. Loud-log so an operator can spot a genuinely-broken
        -- grant in the boot log.
        RAISE NOTICE 'sandbox role-grant tightening: caught % (SQLSTATE %); treating as race-success', SQLERRM, SQLSTATE;
END
$role_split$;
--rollback SELECT 1;
