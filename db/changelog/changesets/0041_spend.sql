--liquibase formatted sql

--changeset zeroship:app-spend-limit splitStatements:true
-- CONFIG: per-app override, written ONLY by set_limit. Absent ⇒ plan default.
CREATE TABLE zeroship.app_spend_limit (
    app_id            UUID PRIMARY KEY REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    spend_limit_cents BIGINT CHECK (spend_limit_cents IS NULL OR spend_limit_cents >= 0),  -- override; NULL clears
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
-- DERIVED: the hot enforcement state, UPSERTed every ~60s; the route-pull LEFT JOINs
-- s.state AS spend_state.
CREATE TABLE zeroship.app_spend_state (
    app_id           UUID PRIMARY KEY REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    state            zeroship.spend_state    NOT NULL DEFAULT 'allow',
    spend_cents      BIGINT                  NOT NULL DEFAULT 0 CHECK (spend_cents >= 0),
    eval_limit_cents BIGINT                  NOT NULL DEFAULT 0 CHECK (eval_limit_cents >= 0),
    period           zeroship.billing_period NOT NULL,
    evaluated_at     TIMESTAMPTZ             NOT NULL DEFAULT NOW()
);
-- Append-only transition audit; typed states; period bound in code.
-- `id` (billing-ops gap #26, PR-6 notifications) is a STABLE surrogate PK = the
-- `billing_notifications.transition_id` for spend-driven notification kinds. A
-- `she_<base62>` typed id minted in Rust by `spend.rs::persist_transition` (NO SQL
-- DEFAULT — there is no in-DB base62 generator, and `gen_random_uuid()` would not
-- carry the `she_` prefix the cross-source dedup-disjointness assertion relies on,
-- design MINOR-3). Added in PR-6 (pre-launch, edited in place — clean re-migrate, no
-- live ALTER). Without it the notifier would dedup on (app_id, at), which is not
-- collision-proof (two transitions can share a timestamp at clock resolution).
CREATE TABLE zeroship.spend_state_history (
    id          TEXT                    PRIMARY KEY,       -- she_<base62> (PR-6)
    app_id      UUID                    NOT NULL REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    period      zeroship.billing_period NOT NULL,
    from_state  zeroship.spend_state    NOT NULL,
    to_state    zeroship.spend_state    NOT NULL,
    spend_cents BIGINT                  NOT NULL CHECK (spend_cents >= 0),
    limit_cents BIGINT                  CHECK (limit_cents IS NULL OR limit_cents >= 0),
    at          TIMESTAMPTZ             NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_spend_state_history_app_at ON zeroship.spend_state_history (app_id, at DESC);
CREATE INDEX idx_spend_state_history_period ON zeroship.spend_state_history (period);
--rollback DROP INDEX IF EXISTS zeroship.idx_spend_state_history_period;
--rollback DROP INDEX IF EXISTS zeroship.idx_spend_state_history_app_at;
--rollback DROP TABLE zeroship.spend_state_history;
--rollback DROP TABLE zeroship.app_spend_state;
--rollback DROP TABLE zeroship.app_spend_limit;

--changeset zeroship:app-spend-rls splitStatements:true
ALTER TABLE zeroship.app_spend_limit     ENABLE ROW LEVEL SECURITY;
ALTER TABLE zeroship.app_spend_limit     FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON zeroship.app_spend_limit
    USING      (app_id = current_setting('zeroship.tenant_app', true)::uuid)
    WITH CHECK (app_id = current_setting('zeroship.tenant_app', true)::uuid);
-- MINOR-4: USING-only (no WITH CHECK) is INTENTIONAL on the writer-restricted
-- tables below. Under FORCE ROW LEVEL SECURITY, Postgres applies the USING
-- predicate as the WITH CHECK fallback for INSERT/UPDATE, so a write whose app_id
-- doesn't match the tenant is still rejected; control connects BYPASSRLS, so these
-- tables are only ever written by the platform. A separate WITH CHECK clause would
-- be redundant here — do NOT add one unless a non-bypass writer is introduced.
ALTER TABLE zeroship.app_spend_state     ENABLE ROW LEVEL SECURITY;
ALTER TABLE zeroship.app_spend_state     FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON zeroship.app_spend_state
    USING (app_id = current_setting('zeroship.tenant_app', true)::uuid);  -- USING-as-CHECK fallback (MINOR-4)
ALTER TABLE zeroship.spend_state_history ENABLE ROW LEVEL SECURITY;
ALTER TABLE zeroship.spend_state_history FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON zeroship.spend_state_history
    USING (app_id = current_setting('zeroship.tenant_app', true)::uuid);  -- USING-as-CHECK fallback (MINOR-4)
--rollback DROP POLICY IF EXISTS tenant_isolation ON zeroship.spend_state_history;
--rollback ALTER TABLE zeroship.spend_state_history NO FORCE ROW LEVEL SECURITY;
--rollback ALTER TABLE zeroship.spend_state_history DISABLE ROW LEVEL SECURITY;
--rollback DROP POLICY IF EXISTS tenant_isolation ON zeroship.app_spend_state;
--rollback ALTER TABLE zeroship.app_spend_state NO FORCE ROW LEVEL SECURITY;
--rollback ALTER TABLE zeroship.app_spend_state DISABLE ROW LEVEL SECURITY;
--rollback DROP POLICY IF EXISTS tenant_isolation ON zeroship.app_spend_limit;
--rollback ALTER TABLE zeroship.app_spend_limit NO FORCE ROW LEVEL SECURITY;
--rollback ALTER TABLE zeroship.app_spend_limit DISABLE ROW LEVEL SECURITY;

--changeset zeroship:app-spend-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.app_spend_limit     TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.app_spend_state     TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, DELETE ON zeroship.spend_state_history TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.app_spend_limit FROM zeroship_control'; EXECUTE 'REVOKE ALL ON zeroship.app_spend_state FROM zeroship_control'; EXECUTE 'REVOKE ALL ON zeroship.spend_state_history FROM zeroship_control'; END IF; END $rb$;
