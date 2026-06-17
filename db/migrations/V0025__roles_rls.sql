-- ════════════════════════════════════════════════════════════════════════
-- Platform role model + Row-Level Security for the `zeroship.*` system tables.
-- ════════════════════════════════════════════════════════════════════════
--
-- Until now EVERY service connected as the `postgres` SUPERUSER
-- (ops/postgres-init.sql), so the per-table REVOKEs in 0002/0004 and the five
-- role names they reference (zeroship_{auth,control,gateway,worker,app}) were
-- non-load-bearing: the roles were never CREATEd, and a superuser bypasses
-- every grant + every RLS policy anyway. This changeset makes the role model
-- REAL:
--
--   1. CREATE the five service login roles (dev passwords; role-existence
--      guarded like the sandbox roles in 0011) with `zeroship` on their
--      search_path so unqualified table references resolve.
--   2. GRANT each role the LEAST-PRIVILEGE set of table verbs its code actually
--      runs (the GRANT MATRIX below is derived from reading every SQL statement
--      each service issues).
--   3. Mark `zeroship_auth` + `zeroship_control` BYPASSRLS — both are
--      LEGITIMATELY cross-tenant (auth's relay reverse-lookup keyed on
--      relay_email alone + cross-app password-reset session fan-out; control's
--      admin delete_app whose FK cascade DELETEs child rows in the RLS tables).
--   4. ENABLE + FORCE ROW LEVEL SECURITY on the four tenant tables
--      (app_secrets, gateway_sessions, app_session_anchors, app_user_identities)
--      and install one `tenant_isolation` policy each, keyed on a per-request
--      GUC the gateway sets via `SET LOCAL` (set_config(..., true)) inside a
--      transaction. The gateway is the ONLY RLS-gated role; it always has the
--      single-tenant key (route.app_id / route.oauth_client_id) in hand.
--
-- Once the roles EXIST, the dead REVOKE-loops in 0002/0004 (which already list
-- all five names) become live and enforce the append-only revokes on them.
--
-- Pre-launch, no back-compat: dev DBs reset, so this is additive DDL with no
-- backfill. The worker's `--db` connection is INTENTIONALLY NOT moved onto the
-- constrained `zeroship_worker` role — plugin-db's register_model provisions
-- per-app PG roles + schemas and so needs a CREATEROLE/CREATEDB provisioning
-- principal, a separate concern from the system-table grant model here.
-- `zeroship_worker` is still CREATEd (it owns zero system-table grants) so the
-- 0002/0004 REVOKE-loops cover it and a future worker split can adopt it.
--
-- ── GRANT MATRIX (S/I/U/D = SELECT/INSERT/UPDATE/DELETE) ─────────────────
-- Append-only tables (audit_events, app_audit, authz_decisions) keep their
-- existing tamper triggers + UPDATE/DELETE/TRUNCATE revokes (0002/0004); the
-- verbs below are the additive grants only.
--
--   zeroship_auth   (BYPASSRLS): users SIUD, federated_identities SID,
--     idp_sessions SIUD, oauth_grants SIU, oauth_clients S, app_scope_defs S,
--     jwk_key_state SID, magic_links SIUD, magic_completions SIUD,
--     email_verifications SIUD, email_suppressions SI, rate_limits SID,
--     cron_state SI, audit_events SID (append-only trigger still gates it; the
--     sanctioned retention DELETE is GUC-flagged), app_user_identities SU
--     (RLS, bypassed),
--     gateway_sessions SD (RLS, bypassed: cross-tenant password-reset; the
--     reset DELETE filters on user_id so SELECT accompanies DELETE).
--   zeroship_control (BYPASSRLS): apps SIUD, app_usage SI, app_usage_history SI,
--     app_vars SID, app_secrets SIUD (RLS, bypassed), app_env_expose SID,
--     oauth_clients SIUD, oauth_grants SID, app_oauth_clients SI,
--     app_scope_defs SID, creator_accounts SIU, creator_account_history SIU,
--     payouts SI, permission_tokens SIU, platform_policies SID,
--     platform_admin_roles SID, users S, rate_limits SD, app_audit SI,
--     app_user_identities SU (RLS, bypassed), token_revocations SID,
--     app_members SIUD.
--     (DELETE/UPDATE that filter on columns carry SELECT too — a WHERE-clause
--      column read requires SELECT even for a pure DELETE/UPDATE.)
--   zeroship_gateway (RLS-GATED, no bypass): gateway_sessions SIU (RLS),
--     app_session_anchors SIUD (RLS), app_user_identities SIU (RLS),
--     token_revocations SID, audit_events I.
--   zeroship_worker: no system-table grants (see note above).
--   zeroship_app: no system-table grants (deny-by-absence backstop).
--
-- Flagged for live-stack validation (could NOT be confidently mapped to a
-- minimal grant from static reads — verify against a running stack):
--   * control's metering/stripe/permission-token surface is broad; the grants
--     above cover every table the registry + handlers reference that this
--     migration's table set knows about, but a handler that touches a table not
--     created in 0002/0004 (or via core wrappers) would need its grant added.
--   * authz_decisions writer role: written via core wrappers; not grant-mapped
--     here (left on PUBLIC-INSERT until the writer's role is pinned).

-- ─── Roles ───────────────────────────────────────────────────────────────
-- splitStatements:false: the DO block carries `;` inside `$bootstrap$`.
--   We mark this ANY because the no-CREATEROLE branch was changed from a silent
--   RAISE NOTICE/RETURN to a fail-loud RAISE EXCEPTION (finding I3, atomicity).
--   That is a deliberate logic change to an already-applied changeset; ANY lets
--   an existing DB re-migrate without a checksum failure (the role state it
--   produced is unchanged on the happy CREATEROLE path the migrate service uses).
DO $bootstrap$
DECLARE
    can_create_role BOOLEAN;
BEGIN
    SELECT rolcreaterole INTO can_create_role
      FROM pg_roles
     WHERE rolname = current_user;

    IF NOT can_create_role THEN
        -- FAIL LOUD (do NOT silently RETURN). If the migration principal
        -- cannot create the service roles, the RLS changesets that follow
        -- would still FORCE row-level security on the four tenant tables —
        -- leaving a HALF-APPLIED LOCKOUT: RLS forced while the BYPASSRLS
        -- roles (zeroship_auth, zeroship_control) that the cross-tenant
        -- services rely on never exist. Aborting here makes role-creation
        -- and RLS-force ATOMIC: either both apply or the migration rolls
        -- back, so no path can reach the lockout state.
        --
        -- The shipped `migrate` service runs as `postgres` (has CREATEROLE),
        -- so this never fires in the delivered config. A managed-DB bootstrap
        -- principal MUST carry CREATEROLE; provision the roles out of band
        -- first (these CREATEs then no-op) rather than running the migration
        -- under a principal that cannot establish the role model.
        RAISE EXCEPTION 'platform role creation requires CREATEROLE: % lacks it. '
            'RLS would be force-enabled without the BYPASSRLS service roles '
            '(half-applied lockout). Provision the zeroship_* roles via a '
            'privileged principal, then re-run.', current_user;
    END IF;

    -- Dev login roles. Passwords are weak DEV defaults matching docker-compose;
    -- production provisions these roles (and rotates passwords) out of band, at
    -- which point these CREATEs no-op (role already exists).
    --
    -- BYPASSRLS for auth + control (both legitimately cross-tenant). gateway +
    -- worker + app are NOT bypass: gateway is the RLS-enforced role, worker/app
    -- own no system-table grants.
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_auth') THEN
        EXECUTE 'CREATE ROLE zeroship_auth LOGIN PASSWORD ''zeroship_auth'' BYPASSRLS';
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_control') THEN
        EXECUTE 'CREATE ROLE zeroship_control LOGIN PASSWORD ''zeroship_control'' BYPASSRLS';
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_gateway') THEN
        EXECUTE 'CREATE ROLE zeroship_gateway LOGIN PASSWORD ''zeroship_gateway''';
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_worker') THEN
        EXECUTE 'CREATE ROLE zeroship_worker LOGIN PASSWORD ''zeroship_worker''';
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_app') THEN
        EXECUTE 'CREATE ROLE zeroship_app LOGIN PASSWORD ''zeroship_app''';
    END IF;

    -- Every connection needs `zeroship` first on its search_path so the
    -- unqualified table references some handlers use (e.g. control's
    -- `UPDATE apps …`) resolve — the same reason ops/postgres-init.sql sets it
    -- on `postgres`.
    EXECUTE 'ALTER ROLE zeroship_auth    SET search_path = zeroship, public';
    EXECUTE 'ALTER ROLE zeroship_control SET search_path = zeroship, public';
    EXECUTE 'ALTER ROLE zeroship_gateway SET search_path = zeroship, public';
    EXECUTE 'ALTER ROLE zeroship_worker  SET search_path = zeroship, public';
    EXECUTE 'ALTER ROLE zeroship_app     SET search_path = zeroship, public';

    -- Schema usage. CREATE is withheld from every service role (DDL is
    -- Liquibase-only, run as the migration principal); USAGE lets them resolve
    -- + use existing objects.
    EXECUTE 'GRANT USAGE ON SCHEMA zeroship TO zeroship_auth, zeroship_control, zeroship_gateway, zeroship_worker, zeroship_app';

    -- Sequence USAGE for the SERIAL `audit_events.id` (the only sequence in the
    -- schema today). INSERTing into a serial column needs USAGE on its
    -- sequence — the table-level INSERT grant alone is insufficient. Only the
    -- roles that INSERT into audit_events (auth + gateway) get it; granting
    -- "ALL SEQUENCES" keeps it correct if a future serial column is added.
    EXECUTE 'GRANT USAGE ON ALL SEQUENCES IN SCHEMA zeroship TO zeroship_auth, zeroship_gateway';

    -- ── zeroship_auth grants ──────────────────────────────────────────────
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.users TO zeroship_auth';
    EXECUTE 'GRANT SELECT, INSERT, DELETE         ON zeroship.federated_identities TO zeroship_auth';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.idp_sessions TO zeroship_auth';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE         ON zeroship.oauth_grants TO zeroship_auth';
    EXECUTE 'GRANT SELECT                         ON zeroship.oauth_clients TO zeroship_auth';
    EXECUTE 'GRANT SELECT                         ON zeroship.app_scope_defs TO zeroship_auth';
    EXECUTE 'GRANT SELECT, INSERT, DELETE         ON zeroship.jwk_key_state TO zeroship_auth';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.magic_links TO zeroship_auth';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.magic_completions TO zeroship_auth';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.email_verifications TO zeroship_auth';
    EXECUTE 'GRANT SELECT, INSERT                 ON zeroship.email_suppressions TO zeroship_auth';
    EXECUTE 'GRANT SELECT, INSERT, DELETE         ON zeroship.rate_limits TO zeroship_auth';
    EXECUTE 'GRANT SELECT, INSERT                 ON zeroship.cron_state TO zeroship_auth';
    -- audit_events is append-only (0002 tamper trigger). Auth INSERTs events
    -- and runs the SANCTIONED retention DELETE (the cron flags its connection
    -- with `SET zeroship.audit_retention = ''on''`, which the trigger allows;
    -- the WHERE on event_type/occurred_at also needs SELECT). The trigger — not
    -- the grant — is the append-only guard; the DELETE privilege only enables
    -- the one GUC-flagged sweep.
    EXECUTE 'GRANT SELECT, INSERT, DELETE         ON zeroship.audit_events TO zeroship_auth';
    -- RLS tables (auth is BYPASSRLS): relay reverse-lookup + cross-app
    -- password-reset session DELETE. SELECT accompanies DELETE because the
    -- reset DELETE filters on `user_id` (a WHERE-clause column read needs
    -- SELECT, even for a pure DELETE).
    EXECUTE 'GRANT SELECT, UPDATE                 ON zeroship.app_user_identities TO zeroship_auth';
    EXECUTE 'GRANT SELECT, DELETE                 ON zeroship.gateway_sessions TO zeroship_auth';

    -- ── zeroship_control grants ───────────────────────────────────────────
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.apps TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT                 ON zeroship.app_usage TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT                 ON zeroship.app_usage_history TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, DELETE         ON zeroship.app_vars TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, DELETE         ON zeroship.app_env_expose TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.oauth_clients TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, DELETE         ON zeroship.oauth_grants TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT                 ON zeroship.app_oauth_clients TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, DELETE         ON zeroship.app_scope_defs TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE         ON zeroship.creator_accounts TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE         ON zeroship.creator_account_history TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT                 ON zeroship.payouts TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE         ON zeroship.permission_tokens TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, DELETE         ON zeroship.platform_policies TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, DELETE         ON zeroship.platform_admin_roles TO zeroship_control';
    EXECUTE 'GRANT SELECT                         ON zeroship.users TO zeroship_control';
    -- SELECT accompanies DELETE: the rate-limit cleanup DELETEs WHERE
    -- bucket_key = $1 (a WHERE-clause column read needs SELECT).
    EXECUTE 'GRANT SELECT, DELETE                 ON zeroship.rate_limits TO zeroship_control';
    EXECUTE 'GRANT INSERT                         ON zeroship.app_audit TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.app_members TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, DELETE         ON zeroship.token_revocations TO zeroship_control';
    -- RLS tables (control is BYPASSRLS): per-app secrets + delete_app cascade.
    -- SELECT accompanies UPDATE on app_user_identities: revoke_grant_cascade
    -- UPDATEs WHERE app_client_id/global_user_id (WHERE-clause column reads).
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.app_secrets TO zeroship_control';
    EXECUTE 'GRANT SELECT, UPDATE                 ON zeroship.app_user_identities TO zeroship_control';

    -- ── zeroship_gateway grants (the RLS-enforced role) ───────────────────
    EXECUTE 'GRANT SELECT, INSERT, UPDATE         ON zeroship.gateway_sessions TO zeroship_gateway';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.app_session_anchors TO zeroship_gateway';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE         ON zeroship.app_user_identities TO zeroship_gateway';
    EXECUTE 'GRANT SELECT, INSERT, DELETE         ON zeroship.token_revocations TO zeroship_gateway';
    EXECUTE 'GRANT INSERT                         ON zeroship.audit_events TO zeroship_gateway';

    -- zeroship_worker + zeroship_app: NO system-table grants (deny-by-absence).
EXCEPTION
    WHEN insufficient_privilege THEN
        -- FAIL LOUD: a grant/create failing mid-block means a misconfigured
        -- migration principal. Re-raise rather than swallow so the migration
        -- aborts and rolls back instead of leaving a partially-applied role
        -- model (the shipped `postgres` principal never trips this).
        RAISE EXCEPTION 'platform role provisioning failed for %: insufficient privilege. '
            'The migration principal must be able to CREATE ROLE and GRANT on '
            'schema zeroship; provision the zeroship_* roles via a privileged '
            'principal, then re-run.', current_user;
END
$bootstrap$;

-- ─── RLS: app_secrets (key: app_id UUID; GUC zeroship.tenant_app) ─────────
-- FORCE so even the table owner is constrained. The policy fails CLOSED: an
-- unset GUC → current_setting(..., true) returns NULL → predicate NULL →
-- zero rows (never an error). The gateway never touches app_secrets, but
-- enabling RLS uniformly across the four tenant tables means a future
-- gateway-role read of app_secrets is auto-confined; control/auth bypass.
ALTER TABLE zeroship.app_secrets ENABLE ROW LEVEL SECURITY;
ALTER TABLE zeroship.app_secrets FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON zeroship.app_secrets
    USING (app_id = current_setting('zeroship.tenant_app', true)::uuid)
    WITH CHECK (app_id = current_setting('zeroship.tenant_app', true)::uuid);

-- ─── RLS: gateway_sessions (key: app_id UUID; GUC zeroship.tenant_app) ────
ALTER TABLE zeroship.gateway_sessions ENABLE ROW LEVEL SECURITY;
ALTER TABLE zeroship.gateway_sessions FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON zeroship.gateway_sessions
    USING (app_id = current_setting('zeroship.tenant_app', true)::uuid)
    WITH CHECK (app_id = current_setting('zeroship.tenant_app', true)::uuid);

-- ─── RLS: app_session_anchors (key: app_id UUID; GUC zeroship.tenant_app) ─
ALTER TABLE zeroship.app_session_anchors ENABLE ROW LEVEL SECURITY;
ALTER TABLE zeroship.app_session_anchors FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON zeroship.app_session_anchors
    USING (app_id = current_setting('zeroship.tenant_app', true)::uuid)
    WITH CHECK (app_id = current_setting('zeroship.tenant_app', true)::uuid);

-- ─── RLS: app_user_identities (key: app_client_id TEXT oac_; GUC
--          zeroship.tenant_client) ────────────────────────────────────────
-- Keyed on the per-app OAuth client_id (the value the gateway always has as
-- route.oauth_client_id), NOT the app uuid — the table has no app_id column.
ALTER TABLE zeroship.app_user_identities ENABLE ROW LEVEL SECURITY;
ALTER TABLE zeroship.app_user_identities FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON zeroship.app_user_identities
    USING (app_client_id = current_setting('zeroship.tenant_client', true))
    WITH CHECK (app_client_id = current_setting('zeroship.tenant_client', true));
