--liquibase formatted sql

-- ════════════════════════════════════════════════════════════════════════
-- Metering-coverage weights (#27 / G4): the two NEW metrics this gap emits
-- in v1 get a `metric_weights` row so they price through the unchanged CU→FX
-- pipeline. The ONLY DB change in G4 — everything else rides `AppUsage.custom`
-- and the raw-metric-keyed `usage_aggregates` (no per-metric DDL).
-- ════════════════════════════════════════════════════════════════════════
--
--   gateway_egress_bytes  the gateway's own egress (static assets, redirects,
--                         gateway error pages) — bodies the worker never sees.
--                         Weighted IDENTICALLY to egress_bytes (1 CU / 1000 B)
--                         so "total egress" is coherent across the worker-owned
--                         `egress_bytes` and the gateway-owned
--                         `gateway_egress_bytes` (the two are disjoint by
--                         construction — the gateway never meters a worker-proxy
--                         body). The operator may diverge later (e.g. CDN-fronted
--                         asset egress) via the runtime weight endpoint.
--   stream_wall_us        the held-open wall time of an SSE/streaming response,
--                         accrued INCREMENTALLY in the worker's stream drain so a
--                         multi-hour stream bills continuously. Mirrors wall_us
--                         (1 CU / 10 ms) — a long-lived stream's wall is priced
--                         like any wall time.
--
-- DEFERRED metrics are intentionally NOT seeded here (matching the 0041 "no
-- db_rows_read weight — no primitive emits it" precedent): the WS names
-- (ws_conn_us / ws_messages / ws_egress_bytes / ws_ingress_bytes) and the
-- db/kv byte names (db_bytes / kv_bytes) get THEIR weight row in the SAME
-- migration that wires their emit point, so the table only ever holds weights
-- for metrics that actually emit. An emitted-but-unweighted metric is free; a
-- weighted-but-unemitted metric is dead config — seed only what emits.
--
-- Pricing is UNCHANGED: `pricing::total_units` reads these rows exactly like
-- the 0041 seeds (total × units_per_op ÷ per_units, then × fx ÷ 10^12). A
-- re-weight reprices history without touching stored usage.

--changeset zeroship:metering-coverage-weights splitStatements:true
INSERT INTO zeroship.metric_weights (metric, units_per_op, per_units) VALUES
    ('gateway_egress_bytes', 1, 1000),
    ('stream_wall_us',       1, 10000)
ON CONFLICT (metric) DO NOTHING;
--rollback DELETE FROM zeroship.metric_weights WHERE metric IN ('gateway_egress_bytes', 'stream_wall_us');
