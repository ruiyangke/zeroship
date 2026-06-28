-- Priced identity. runtime_limits_json STAYS: the 5s route-pull LEFT JOINs it. The
-- billing/sandbox coupling critique is resolved by keeping it OFF the price semantics
-- (plan identity, not a priced field), NOT by removing it. The poison-tolerance fix
-- lives in runtime_limits_from_catalog().
CREATE TABLE zeroship.plans (
    id                        TEXT        PRIMARY KEY,           -- pln_<base62>
    name                      TEXT        NOT NULL,
    base_fee_cents            BIGINT      NOT NULL DEFAULT 0 CHECK (base_fee_cents >= 0),
    included_units            BIGINT      NOT NULL DEFAULT 0 CHECK (included_units >= 0),  -- CU free before overage
    fx_pico_cents_per_unit    BIGINT      CHECK (fx_pico_cents_per_unit IS NULL OR fx_pico_cents_per_unit >= 1000),  -- NULL ⇒ global default
    spend_limit_default_cents BIGINT      NOT NULL DEFAULT 0 CHECK (spend_limit_default_cents >= 0),
    assignable_by_creator     BOOLEAN     NOT NULL DEFAULT false,
    runtime_limits_json       JSONB       NOT NULL,              -- AppRuntimeLimits; identity, NOT a priced field
    archived                  BOOLEAN     NOT NULL DEFAULT false,
    created_at                TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at                TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
ALTER TABLE zeroship.apps
    ADD CONSTRAINT apps_plan_fk FOREIGN KEY (plan_id) REFERENCES zeroship.plans(id) ON DELETE RESTRICT;
CREATE INDEX apps_plan_id_idx ON zeroship.apps(plan_id);

DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.plans TO zeroship_control';
  END IF;
END $g$;
