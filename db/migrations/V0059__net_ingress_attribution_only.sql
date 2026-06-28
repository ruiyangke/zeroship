-- Native raw-TCP ingress is already billed through the platform
-- `ingress_bytes` metric. Keep `net_ingress_bytes` for attribution/abuse
-- visibility, but remove its second billable path.

INSERT INTO zeroship.billing_metrics (metric, kind, unit)
VALUES ('net_ingress_bytes', 'primitive', 'byte')
ON CONFLICT (metric) DO NOTHING;

INSERT INTO zeroship.metric_weights (metric, units_per_op, per_units)
VALUES ('net_ingress_bytes', 0, 10000)
ON CONFLICT (metric) DO UPDATE SET
    units_per_op = EXCLUDED.units_per_op,
    per_units = EXCLUDED.per_units,
    updated_at = NOW();
