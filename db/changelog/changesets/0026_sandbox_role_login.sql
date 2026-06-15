--liquibase formatted sql

-- Make the sandbox role-split roles (created NOLOGIN in 0011) LOGIN with dev
-- passwords, so the sandbox service can connect AS its least-privilege role
-- (P13): SANDBOX_DATABASE_URL → sandbox_app, SANDBOX_DATABASE_URL_AUDIT →
-- sandbox_audit, SANDBOX_DATABASE_URL_GDPR → sandbox_gdpr. Their GRANTs are
-- already established in 0011 (runtime DML / INSERT-only audit / scoped GDPR
-- DELETE); this only flips the login attribute + sets a dev password.
--
-- `sandbox_admin` (the DDL/migration role) deliberately stays NOLOGIN — DDL is
-- run by the Liquibase migration principal, not by a logged-in sandbox process.
--
-- Role-existence + CREATEROLE guarded like 0011/0025 so restricted CI Postgres
-- (no CREATEROLE) degrades silently; production provisions the roles + rotates
-- passwords out of band, at which point ALTER ROLE … LOGIN is a harmless no-op
-- delta.
--
-- splitStatements:false: the DO block carries `;` inside `$bootstrap$`.
--changeset zeroship:sandbox-roles-login splitStatements:false rollbackSplitStatements:false
DO $bootstrap$
DECLARE
    can_create_role BOOLEAN;
BEGIN
    SELECT rolcreaterole INTO can_create_role
      FROM pg_roles
     WHERE rolname = current_user;

    IF NOT can_create_role THEN
        RAISE NOTICE 'sandbox role LOGIN flip skipped: caller lacks CREATEROLE';
        RETURN;
    END IF;

    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_app') THEN
        EXECUTE 'ALTER ROLE sandbox_app   LOGIN PASSWORD ''sandbox_app''';
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_audit') THEN
        EXECUTE 'ALTER ROLE sandbox_audit LOGIN PASSWORD ''sandbox_audit''';
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_gdpr') THEN
        EXECUTE 'ALTER ROLE sandbox_gdpr  LOGIN PASSWORD ''sandbox_gdpr''';
    END IF;

    -- The sandbox tables live in `zeroship`; give the login roles that schema
    -- first on their search_path (the 0011 grants already gave USAGE).
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_app') THEN
        EXECUTE 'ALTER ROLE sandbox_app   SET search_path = zeroship, public';
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_audit') THEN
        EXECUTE 'ALTER ROLE sandbox_audit SET search_path = zeroship, public';
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_gdpr') THEN
        EXECUTE 'ALTER ROLE sandbox_gdpr  SET search_path = zeroship, public';
    END IF;
EXCEPTION
    WHEN insufficient_privilege THEN
        RAISE NOTICE 'sandbox role LOGIN flip skipped: caller lacks privilege';
END
$bootstrap$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_app') THEN EXECUTE 'ALTER ROLE sandbox_app NOLOGIN'; EXECUTE 'ALTER ROLE sandbox_audit NOLOGIN'; EXECUTE 'ALTER ROLE sandbox_gdpr NOLOGIN'; END IF; END $rb$;
