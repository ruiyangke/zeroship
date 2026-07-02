-- Standalone creator migration service state.
--
-- These tables are owned by the control database schema because the migration
-- service authorizes through control-plane principals/apps, then stores
-- app-scoped policy versions, pending approval workflow rows, and immutable
-- audit evidence.

CREATE TABLE IF NOT EXISTS zeroship.migrated_app_policies (
    app_id UUID NOT NULL REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    version BIGINT NOT NULL CHECK (version > 0),
    raw_toml TEXT NOT NULL,
    parsed_profile JSONB NOT NULL,
    effective_profile JSONB NOT NULL,
    ceiling_id TEXT NOT NULL,
    ceiling_version BIGINT NOT NULL CHECK (ceiling_version > 0),
    submitted_by UUID NOT NULL REFERENCES zeroship.users(id) ON DELETE RESTRICT,
    submitted_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (app_id, version)
);

CREATE INDEX IF NOT EXISTS migrated_app_policies_app_submitted_idx
    ON zeroship.migrated_app_policies(app_id, submitted_at DESC);

CREATE TABLE IF NOT EXISTS zeroship.migrated_migrations (
    app_id UUID NOT NULL REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    migration_id UUID NOT NULL,
    status TEXT NOT NULL CHECK (
        status IN ('submitted', 'pending_approval', 'approved', 'applied', 'failed')
    ),
    request_body JSONB NOT NULL,
    effective_profile JSONB NOT NULL,
    ceiling_id TEXT NOT NULL,
    ceiling_version BIGINT NOT NULL CHECK (ceiling_version > 0),
    gated_versions JSONB NOT NULL DEFAULT '[]'::jsonb,
    submitted_by UUID NOT NULL REFERENCES zeroship.users(id) ON DELETE RESTRICT,
    submitted_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    approved_by UUID REFERENCES zeroship.users(id) ON DELETE RESTRICT,
    approved_at TIMESTAMPTZ,
    applied_at TIMESTAMPTZ,
    last_error TEXT,
    PRIMARY KEY (app_id, migration_id)
);

CREATE INDEX IF NOT EXISTS migrated_migrations_app_status_idx
    ON zeroship.migrated_migrations(app_id, status, submitted_at DESC);

CREATE TABLE IF NOT EXISTS zeroship.migrated_migration_audit (
    audit_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    app_id UUID NOT NULL REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    migration_id UUID NOT NULL,
    migration_versions JSONB NOT NULL DEFAULT '[]'::jsonb,
    action TEXT NOT NULL CHECK (action IN ('submit', 'reject_pending', 'approve', 'apply')),
    outcome TEXT NOT NULL,
    principal_id UUID NOT NULL REFERENCES zeroship.users(id) ON DELETE RESTRICT,
    effective_profile JSONB NOT NULL,
    sealed_profile JSONB,
    ceiling_id TEXT NOT NULL,
    ceiling_version BIGINT NOT NULL CHECK (ceiling_version > 0),
    detail JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS migrated_migration_audit_app_idx
    ON zeroship.migrated_migration_audit(app_id, migration_id, created_at ASC);

CREATE OR REPLACE FUNCTION zeroship.reject_migrated_migration_audit_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'migrated_migration_audit is append-only';
END;
$$;

DROP TRIGGER IF EXISTS migrated_migration_audit_append_only
    ON zeroship.migrated_migration_audit;
CREATE TRIGGER migrated_migration_audit_append_only
    BEFORE UPDATE OR DELETE ON zeroship.migrated_migration_audit
    FOR EACH ROW EXECUTE FUNCTION zeroship.reject_migrated_migration_audit_mutation();

COMMENT ON TABLE zeroship.migrated_app_policies IS
    'Creator migration policy versions. App isolation is enforced in the migrated service by authorization plus app_id-scoped queries; no table RLS is installed because the service role can hold BYPASSRLS.';

COMMENT ON TABLE zeroship.migrated_migrations IS
    'Creator migration workflow rows. App isolation is enforced by migrated service authorization plus app_id-scoped primary-key lookups.';

COMMENT ON TABLE zeroship.migrated_migration_audit IS
    'Append-only audit history for creator migration submit, approval, pending rejection, and apply outcomes.';

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_control') THEN
        GRANT SELECT, INSERT ON zeroship.migrated_app_policies TO zeroship_control;
        GRANT SELECT, INSERT, UPDATE ON zeroship.migrated_migrations TO zeroship_control;
        GRANT SELECT, INSERT ON zeroship.migrated_migration_audit TO zeroship_control;
    END IF;
END
$$;
