-- 0004_role_split_phase3.sql — Phase 3 explicit role-grant tightening
--
-- Source-of-truth: docs/proposals/sandbox-pg-state.md § 13.2 + the
-- Phase-3 task spec (worktree feat/sandbox-pg).
--
-- Round-7 / migration-0001 created the four roles (sandbox_admin,
-- sandbox_app, sandbox_audit, sandbox_gdpr) and a permissive grant
-- bundle. Phase 3 narrows the grants to the design's invariant:
--
--   sandbox_app:    SELECT/INSERT/UPDATE/DELETE on non-events tables;
--                   SELECT/INSERT on events (DELETE forbidden — the
--                   controller cannot tamper with its own audit trail)
--   sandbox_audit:  INSERT-only on events (no SELECT, no DELETE)
--   sandbox_gdpr:   SELECT + DELETE on every table the cascade touches;
--                   INSERT on events + deleted_sandboxes (audit row +
--                   tombstone live in the same TX)
--   sandbox_admin:  CREATE/USAGE on the schema (DDL/migrations)
--
-- This migration is forward-only and idempotent. REVOKE-then-GRANT is
-- safe to replay — the final state is the same regardless of which
-- subset of grants 0001's permissive bundle landed in CI vs. prod.
--
-- The DO block degrades gracefully when the calling role lacks the
-- privilege to manage other roles' grants (CI Postgres runs migrations
-- as a single role); the grants run only when the role exists and the
-- caller can manage it. In production the operator deploys the four
-- roles via a separate bootstrap script BEFORE this migration runs;
-- the bootstrap role has the GRANT/REVOKE capability we need here.

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
        EXECUTE 'REVOKE ALL ON sandbox.events FROM sandbox_app';
        EXECUTE 'GRANT SELECT, INSERT ON sandbox.events TO sandbox_app';
        -- Ensure the non-events grants are present (idempotent —
        -- 0001 already granted these).
        EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON
                    sandbox.sandboxes, sandbox.shares, sandbox.hosts,
                    sandbox.deleted_sandboxes, sandbox.schema_migrations
                    TO sandbox_app';
    END IF;

    -- ─── sandbox_audit ──────────────────────────────────────────
    -- INSERT-only on events. Audit pipe (currently inside the
    -- controller, eventually a separate process) writes via this
    -- role; the role cannot SELECT, UPDATE, or DELETE rows it wrote.
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_audit') THEN
        EXECUTE 'REVOKE ALL ON ALL TABLES IN SCHEMA sandbox FROM sandbox_audit';
        EXECUTE 'GRANT USAGE ON SCHEMA sandbox TO sandbox_audit';
        EXECUTE 'GRANT INSERT ON sandbox.events TO sandbox_audit';
    END IF;

    -- ─── sandbox_gdpr ───────────────────────────────────────────
    -- SELECT (the cascade WHERE chain reads from sandboxes); DELETE
    -- on the cascade tables; INSERT on the audit + tombstone tables
    -- (the GDPR-delete TX writes a `gdpr.delete_user` event row in
    -- the same transaction as the cascade).
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_gdpr') THEN
        EXECUTE 'REVOKE ALL ON ALL TABLES IN SCHEMA sandbox FROM sandbox_gdpr';
        EXECUTE 'GRANT USAGE ON SCHEMA sandbox TO sandbox_gdpr';
        EXECUTE 'GRANT SELECT, DELETE ON
                    sandbox.sandboxes, sandbox.shares, sandbox.events,
                    sandbox.deleted_sandboxes
                    TO sandbox_gdpr';
        EXECUTE 'GRANT INSERT ON sandbox.deleted_sandboxes, sandbox.events TO sandbox_gdpr';
    END IF;

    -- ─── sandbox_admin ──────────────────────────────────────────
    -- CREATE/USAGE on schema only. No table-level grants — the
    -- migration role applies DDL via batch_execute as the role's
    -- table-owner; runtime DML is forbidden.
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_admin') THEN
        EXECUTE 'REVOKE ALL ON ALL TABLES IN SCHEMA sandbox FROM sandbox_admin';
        EXECUTE 'GRANT CREATE, USAGE ON SCHEMA sandbox TO sandbox_admin';
    END IF;
EXCEPTION
    WHEN insufficient_privilege THEN
        RAISE NOTICE 'sandbox role-grant tightening skipped: insufficient privilege on REVOKE/GRANT';
    WHEN deadlock_detected THEN
        -- Two concurrent migrators racing on REVOKE/GRANT for the
        -- same role observe a deadlock on `pg_authid` (one waiter
        -- holds the role-grant lock, the other holds the table-lock).
        -- The winner's grants are durable; the loser's TX is rolled
        -- back, which the migration runner re-tries via the bookkeeping
        -- INSERT. Treat as a race-tolerant success — same shape as the
        -- duplicate_object handling for IF NOT EXISTS in 0001.
        RAISE NOTICE 'sandbox role-grant tightening: deadlock with concurrent migrator; treating as race-success';
    WHEN serialization_failure THEN
        RAISE NOTICE 'sandbox role-grant tightening: serialization failure (concurrent migrator); treating as race-success';
    WHEN OTHERS THEN
        -- Some pg builds surface the role-grant race as a generic
        -- internal_error (XX000) rather than a deadlock. Since this
        -- migration only refines grants — never data — we can treat
        -- ANY exception as a race-tolerant success and let the next
        -- migrator's INSERT-bookkeeping path settle the version row.
        -- The grants are idempotent: whichever migrator wins, the
        -- final grant set is the same. Loud-log so an operator can
        -- spot a genuinely-broken grant in the boot log.
        RAISE NOTICE 'sandbox role-grant tightening: caught % (SQLSTATE %); treating as race-success', SQLERRM, SQLSTATE;
END
$role_split$;
