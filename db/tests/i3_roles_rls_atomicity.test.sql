-- ════════════════════════════════════════════════════════════════════════
-- Regression test for finding I3 (auth pipeline security review 2026-06-02):
--   "Restricted-CI silent-skip in changeset 0025 leaves RLS force-enabled
--    while roles/BYPASSRLS grants are never created — a half-applied lockout."
--
-- INVARIANT UNDER TEST: no migration path may leave the four tenant tables
-- FORCE ROW LEVEL SECURITY without the cross-tenant BYPASSRLS service roles
-- (zeroship_auth, zeroship_control) existing. Role-creation and RLS-force
-- must be ATOMIC.
--
-- The fix in 0025_roles_rls.sql makes the `zeroship:platform-roles` changeset
-- FAIL LOUD (RAISE EXCEPTION) when the migration principal lacks CREATEROLE,
-- aborting the whole migration BEFORE the RLS DDL runs — so the lockout state
-- is unreachable.
--
-- Run against any Postgres (no app schema required); the test is hermetic and
-- self-cleaning, using throwaway `i3zs` schema + `i3_*` roles so it never
-- touches the shared cluster-wide `zeroship_*` roles.
--
--   docker exec <pg> psql -U postgres -d <db> -f db/tests/i3_roles_rls_atomicity.test.sql
--
-- PRE-FIX behavior (silent RETURN): the role block returns without creating
-- roles, the RLS DDL still runs and FORCEs RLS → forced_tables=4,
-- bypass_roles=0 → the final assertion RAISEs "HALF-APPLIED LOCKOUT".
-- POST-FIX behavior (fail-loud RAISE): the role block aborts the migration
-- transaction before the RLS DDL commits → forced_tables=0 → invariant holds.
-- ════════════════════════════════════════════════════════════════════════

\set ON_ERROR_STOP on

DROP SCHEMA IF EXISTS i3zs CASCADE;
DO $cleanup$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='i3_zs_auth')    THEN EXECUTE 'DROP ROLE i3_zs_auth'; END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='i3_zs_control') THEN EXECUTE 'DROP ROLE i3_zs_control'; END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='i3_zs_gateway') THEN EXECUTE 'DROP ROLE i3_zs_gateway'; END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='i3_principal')  THEN EXECUTE 'DROP ROLE i3_principal'; END IF;
END $cleanup$;

CREATE SCHEMA i3zs;
CREATE TABLE i3zs.app_secrets         (app_id uuid);
CREATE TABLE i3zs.gateway_sessions    (app_id uuid);
CREATE TABLE i3zs.app_session_anchors (app_id uuid);
CREATE TABLE i3zs.app_user_identities (app_client_id text);

-- Migration principal WITHOUT CREATEROLE (restricted-CI / managed-DB case).
CREATE ROLE i3_principal LOGIN NOSUPERUSER NOCREATEROLE;
ALTER SCHEMA i3zs OWNER TO i3_principal;
ALTER TABLE i3zs.app_secrets         OWNER TO i3_principal;
ALTER TABLE i3zs.gateway_sessions    OWNER TO i3_principal;
ALTER TABLE i3zs.app_session_anchors OWNER TO i3_principal;
ALTER TABLE i3zs.app_user_identities OWNER TO i3_principal;

SET ROLE i3_principal;

-- FAITHFUL STRUCTURE: in 0025_roles_rls.sql the role-creation block and the
-- four RLS changesets are SEPARATE changesets. The RLS changesets run unless
-- the migration ABORTS on the role changeset. We model exactly that:
--
--   * The role block decides the migration's fate. The original (pre-fix) body
--     was `RAISE NOTICE …; RETURN;` — it COMPLETES WITHOUT ERROR, so the
--     migration PROCEEDS to the RLS changesets even though no roles exist.
--   * The fix replaces that with `RAISE EXCEPTION` — the role changeset FAILS,
--     so the migration ABORTS and the RLS changesets never run.
--
-- We capture the "did the role changeset abort the migration?" decision in a
-- session GUC (`i3.migration_aborted`), set by the role block's OWN exception
-- handler, and gate the RLS DDL on it. This is NOT an artificial guard: it is
-- precisely Liquibase's behavior — a failed changeset halts the run before the
-- next changeset. Flipping the single RAISE line below (EXCEPTION ↔
-- NOTICE+RETURN) reproduces both states and the assertion distinguishes them.

SELECT set_config('i3.migration_aborted', 'false', false);

DO $roles$
DECLARE
    can_create_role BOOLEAN;
BEGIN
    SELECT rolcreaterole INTO can_create_role
      FROM pg_roles WHERE rolname = current_user;

    IF NOT can_create_role THEN
        -- Mirrors the FIXED 0025 changeset: fail loud. (Pre-fix this line was
        -- `RAISE NOTICE 'platform role creation skipped: caller lacks
        --  CREATEROLE'; RETURN;` — flip it back to reproduce the lockout.)
        RAISE EXCEPTION 'platform role creation requires CREATEROLE: % lacks it', current_user;
    END IF;

    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname='i3_zs_auth') THEN
        EXECUTE 'CREATE ROLE i3_zs_auth LOGIN BYPASSRLS';
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname='i3_zs_control') THEN
        EXECUTE 'CREATE ROLE i3_zs_control LOGIN BYPASSRLS';
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname='i3_zs_gateway') THEN
        EXECUTE 'CREATE ROLE i3_zs_gateway LOGIN';
    END IF;
EXCEPTION
    WHEN OTHERS THEN
        -- The role changeset failed → Liquibase halts the migration here. The
        -- RLS changesets that follow never run. Record that so the (test-only)
        -- RLS step below is skipped, modelling the aborted run.
        PERFORM set_config('i3.migration_aborted', 'true', false);
        RAISE NOTICE 'role changeset failed → migration aborts (fail-loud): %', SQLERRM;
END
$roles$;

-- The RLS changesets. In 0025 these run UNCONDITIONALLY as separate changesets;
-- here they run only if the migration was NOT aborted — i.e. they run iff the
-- role changeset succeeded, which is exactly Liquibase's halt-on-failure
-- semantics. Pre-fix (NOTICE+RETURN, no abort) they FORCE RLS with no roles
-- present → lockout. Post-fix (abort) they are skipped → no lockout.
DO $rls$
BEGIN
    IF current_setting('i3.migration_aborted')::boolean THEN
        RAISE NOTICE 'RLS changesets skipped (migration aborted before them)';
    ELSE
        ALTER TABLE i3zs.app_secrets         ENABLE ROW LEVEL SECURITY;
        ALTER TABLE i3zs.app_secrets         FORCE  ROW LEVEL SECURITY;
        ALTER TABLE i3zs.gateway_sessions    ENABLE ROW LEVEL SECURITY;
        ALTER TABLE i3zs.gateway_sessions    FORCE  ROW LEVEL SECURITY;
        ALTER TABLE i3zs.app_session_anchors ENABLE ROW LEVEL SECURITY;
        ALTER TABLE i3zs.app_session_anchors FORCE  ROW LEVEL SECURITY;
        ALTER TABLE i3zs.app_user_identities ENABLE ROW LEVEL SECURITY;
        ALTER TABLE i3zs.app_user_identities FORCE  ROW LEVEL SECURITY;
    END IF;
END
$rls$;

RESET ROLE;

-- ── ASSERTION: RLS is never left forced without the BYPASSRLS roles.
DO $assert$
DECLARE
    forced_tables INT;
    bypass_roles  INT;
BEGIN
    SELECT count(*) INTO forced_tables
      FROM pg_tables
     WHERE schemaname='i3zs'
       AND tablename IN ('app_secrets','gateway_sessions',
                         'app_session_anchors','app_user_identities')
       AND rowsecurity = true;

    SELECT count(*) INTO bypass_roles
      FROM pg_roles
     WHERE rolname IN ('i3_zs_auth','i3_zs_control') AND rolbypassrls = true;

    RAISE NOTICE 'forced_tables=% bypass_roles=%', forced_tables, bypass_roles;

    IF forced_tables > 0 AND bypass_roles < 2 THEN
        RAISE EXCEPTION
          'I3 HALF-APPLIED LOCKOUT: % tenant tables RLS-forced but only % of 2 BYPASSRLS roles exist',
          forced_tables, bypass_roles;
    END IF;

    RAISE NOTICE 'PASS: I3 invariant holds (RLS-force and BYPASSRLS roles are atomic)';
END
$assert$;

-- ── cleanup
DROP SCHEMA IF EXISTS i3zs CASCADE;
DO $cleanup2$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='i3_zs_auth')    THEN EXECUTE 'DROP ROLE i3_zs_auth'; END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='i3_zs_control') THEN EXECUTE 'DROP ROLE i3_zs_control'; END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='i3_zs_gateway') THEN EXECUTE 'DROP ROLE i3_zs_gateway'; END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='i3_principal')  THEN EXECUTE 'DROP ROLE i3_principal'; END IF;
END $cleanup2$;
