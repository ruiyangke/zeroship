--liquibase formatted sql

--changeset zeroship:metering-exports splitStatements:true
-- Export-only; NEVER feeds enforcement (the spend cap reads usage_aggregates directly —
-- keeping providers pluggable). Cumulative high-water + failure surface kept verbatim.
CREATE TABLE zeroship.metering_exports (
    app_id               UUID                    NOT NULL REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    period               zeroship.billing_period NOT NULL,
    exported_units       BIGINT                  NOT NULL DEFAULT 0 CHECK (exported_units >= 0),  -- cumulative CU (monotonic)
    consecutive_failures INTEGER                 NOT NULL DEFAULT 0 CHECK (consecutive_failures >= 0),
    last_error           TEXT,                                                                    -- redacted at source
    last_attempt_at      TIMESTAMPTZ,
    updated_at           TIMESTAMPTZ             NOT NULL DEFAULT NOW(),
    PRIMARY KEY (app_id, period)
);
--rollback DROP TABLE zeroship.metering_exports;

--changeset zeroship:metering-exports-rls splitStatements:true
-- MINOR-4: USING-only (no WITH CHECK) is INTENTIONAL. Under FORCE ROW LEVEL
-- SECURITY, Postgres applies the USING predicate as the WITH CHECK fallback for
-- INSERT/UPDATE, so a cross-tenant write is rejected anyway; control connects
-- BYPASSRLS and is the only writer. A separate WITH CHECK would be redundant here
-- — do NOT add one unless a non-bypass writer is introduced.
ALTER TABLE zeroship.metering_exports ENABLE ROW LEVEL SECURITY;
ALTER TABLE zeroship.metering_exports FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON zeroship.metering_exports
    USING (app_id = current_setting('zeroship.tenant_app', true)::uuid);  -- USING-as-CHECK fallback (MINOR-4)
--rollback DROP POLICY IF EXISTS tenant_isolation ON zeroship.metering_exports;
--rollback ALTER TABLE zeroship.metering_exports NO FORCE ROW LEVEL SECURITY;
--rollback ALTER TABLE zeroship.metering_exports DISABLE ROW LEVEL SECURITY;

--changeset zeroship:metering-exports-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.metering_exports TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.metering_exports FROM zeroship_control'; END IF; END $rb$;
