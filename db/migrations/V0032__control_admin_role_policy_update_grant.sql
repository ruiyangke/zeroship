-- Grant zeroship_control UPDATE on the two platform-admin tables (security
-- finding F6). The admin handlers grant_platform_role / upsert_platform_policy
-- use `INSERT ... ON CONFLICT DO UPDATE`, which Postgres permission-checks for
-- UPDATE even on a non-conflicting first insert. Changeset 0025 granted
-- zeroship_control only SELECT/INSERT/DELETE on platform_admin_roles /
-- platform_policies, so every POST /admin/users/{id}/role and
-- PUT /admin/platform-policies/{id} returned 500 (permission denied). The
-- control service runs as zeroship_control (docker-compose), so this bricked
-- admin role/policy management. Add the missing verb here in its own new
-- migration file (not by editing the already-applied 0025) so existing DBs
-- re-migrate cleanly.

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_control') THEN
        EXECUTE 'GRANT UPDATE ON zeroship.platform_admin_roles TO zeroship_control';
        EXECUTE 'GRANT UPDATE ON zeroship.platform_policies     TO zeroship_control';
    END IF;
END $$;
