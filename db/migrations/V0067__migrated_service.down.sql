DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_control') THEN
        REVOKE ALL ON zeroship.migrated_migration_audit FROM zeroship_control;
        REVOKE ALL ON zeroship.migrated_migrations FROM zeroship_control;
        REVOKE ALL ON zeroship.migrated_app_policies FROM zeroship_control;
    END IF;
END
$$;

DROP TRIGGER IF EXISTS migrated_migration_audit_append_only
    ON zeroship.migrated_migration_audit;
DROP FUNCTION IF EXISTS zeroship.reject_migrated_migration_audit_mutation();
DROP TABLE IF EXISTS zeroship.migrated_migration_audit;
DROP TABLE IF EXISTS zeroship.migrated_migrations;
DROP TABLE IF EXISTS zeroship.migrated_app_policies;
