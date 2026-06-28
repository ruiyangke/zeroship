-- Operator-authored creator outbound raw-TCP grants.
--
-- The manifest may request host:port pairs, but enforcement is table-authoritative:
-- only rows in app_net_grants become worker-facing AppNetPolicy entries.
-- Pre-launch: additive catalog/table shape, no compatibility shims.

CREATE TABLE zeroship.app_net_grants (
    app_id     UUID        NOT NULL REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    host       TEXT        NOT NULL,
    port       INT         NOT NULL CHECK (port BETWEEN 1 AND 65535),
    granted_by TEXT        NOT NULL,
    granted_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    note       TEXT,
    PRIMARY KEY (app_id, host, port)
);

CREATE INDEX app_net_grants_app_id_idx ON zeroship.app_net_grants (app_id);

-- Operator-editable net-policy catalog. The runtime keeps a compiled-in
-- frontable-suffix backstop; this row lets operators add more suffixes without
-- a binary rollout. If the row is missing/corrupt, wildcard authoring fails
-- closed in the control-plane validator.
CREATE TABLE zeroship.net_policy_catalog (
    key        TEXT        PRIMARY KEY,
    value_json JSONB       NOT NULL,
    updated_by TEXT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

INSERT INTO zeroship.net_policy_catalog (key, value_json, updated_by)
VALUES (
    'frontable_wildcard_suffixes',
    '[
      "workers.dev",
      "pages.dev",
      "vercel.app",
      "netlify.app",
      "herokuapp.com",
      "fly.dev",
      "railway.app",
      "render.com",
      "onrender.com",
      "neon.tech",
      "supabase.co",
      "amazonaws.com",
      "cloudfront.net"
    ]'::jsonb,
    'migration:V0058'
)
ON CONFLICT (key) DO NOTHING;

-- Plan-tier TCP caps: hosts are per-app grants, limits are tier properties.
ALTER TABLE zeroship.plans
    ADD COLUMN net_policy_limits_json JSONB NOT NULL DEFAULT
        '{"max_sockets":4,"egress_ceiling_bytes":10485760}'::jsonb;

-- S3: native raw-TCP ingress becomes billable once grants make downloads
-- reachable. Weight it like platform ingress_bytes (1 CU / 10 KiB).
INSERT INTO zeroship.billing_metrics (metric, kind, unit)
VALUES ('net_ingress_bytes', 'primitive', 'byte')
ON CONFLICT (metric) DO NOTHING;

INSERT INTO zeroship.metric_weights (metric, units_per_op, per_units)
VALUES ('net_ingress_bytes', 1, 10000)
ON CONFLICT (metric) DO UPDATE SET
    units_per_op = EXCLUDED.units_per_op,
    per_units = EXCLUDED.per_units,
    updated_at = NOW();

DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.app_net_grants TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.net_policy_catalog TO zeroship_control';
  END IF;
END $g$;
