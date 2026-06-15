#!/usr/bin/env bash
#
# metering_rowlock_pgbench.sh — ISOLATE the usage_aggregates hot-row UPSERT
# contention (#29) at the DB level, with PERSISTENT POOLED connections.
#
# The Rust harness (crates/control/benches/metering_load.rs, BENCH 1) drives the
# REAL Metering::ingest_at path, but the no-pool `Registry` opens a fresh PG
# connection PER report — so per-report connection-open overhead (~10-15ms)
# dominates and MASKS the row-lock cost at the concurrency the dev box sustains
# (≤32, capped by max_connections=100). This script removes that mask: pgbench
# holds one persistent connection per client and hammers the bare UPSERT, so the
# measured tps/latency delta between HOT (every client → ONE row) and SPREAD
# (each tx → a random row of 10k) is PURELY the row-lock serialization tax.
#
# It writes to a DEDICATED throwaway table (public.ml_pgbench_agg) mirroring the
# usage_aggregates PK + UPSERT shape; it never touches usage_aggregates itself.
# Self-cleaning: drops the table on exit.
#
# Usage:
#   METERING_LOAD_DB='postgres://postgres:zeroship@localhost:5440/zeroship_metering_load' \
#     tests/metering_rowlock_pgbench.sh
#
#   PSQL=/path/to/psql PGBENCH=/path/to/pgbench  # override tool paths
#   DURATION=5  CLIENTS="1 4 8 16 32 48"          # override sweep
#
# Requires `psql` + `pgbench` on PATH (or via PSQL/PGBENCH). DO NOT point this at
# the real `zeroship` DB; the table is created in `public` and dropped after.
set -euo pipefail

DB_URL="${METERING_LOAD_DB:-postgres://postgres:zeroship@localhost:5440/zeroship_metering_load}"
PSQL="${PSQL:-psql}"
PGBENCH="${PGBENCH:-pgbench}"
DURATION="${DURATION:-5}"
CLIENTS="${CLIENTS:-1 4 8 16 32 48}"

case "$DB_URL" in
  *zeroship_metering_load*) : ;;
  *) echo "refusing: METERING_LOAD_DB must name the dedicated zeroship_metering_load DB" >&2; exit 2 ;;
esac

TMP="$(mktemp -d)"
cleanup() {
  "$PSQL" "$DB_URL" -q -c "DROP TABLE IF EXISTS public.ml_pgbench_agg" >/dev/null 2>&1 || true
  rm -rf "$TMP"
}
trap cleanup EXIT

echo "=== seeding throwaway hot/spread table (10k rows) ==="
"$PSQL" "$DB_URL" -q <<'SQL'
DROP TABLE IF EXISTS public.ml_pgbench_agg;
CREATE TABLE public.ml_pgbench_agg (
  app_id uuid NOT NULL,
  period date NOT NULL,
  metric text NOT NULL,
  total bigint NOT NULL DEFAULT 0,
  updated_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (app_id, period, metric)
);
INSERT INTO public.ml_pgbench_agg (app_id, period, metric, total)
SELECT ('00000000-0000-0000-0000-' || lpad(g::text, 12, '0'))::uuid,
       DATE '2030-03-01', 'requests', 0
FROM generate_series(0, 9999) g;
SQL

cat > "$TMP/hot.sql" <<'SQL'
-- HOT: every client UPSERTs the SAME row → full row-lock serialization.
INSERT INTO public.ml_pgbench_agg AS u (app_id, period, metric, total, updated_at)
VALUES ('00000000-0000-0000-0000-000000000000'::uuid, DATE '2030-03-01', 'requests', 1, now())
ON CONFLICT (app_id, period, metric)
DO UPDATE SET total = u.total + EXCLUDED.total, updated_at = now();
SQL

cat > "$TMP/spread.sql" <<'SQL'
-- SPREAD: each tx targets a RANDOM row of 10000 → contention dispersed.
\set rid random(0, 9999)
INSERT INTO public.ml_pgbench_agg AS u (app_id, period, metric, total, updated_at)
VALUES (('00000000-0000-0000-0000-' || lpad(:rid::text,12,'0'))::uuid, DATE '2030-03-01', 'requests', 1, now())
ON CONFLICT (app_id, period, metric)
DO UPDATE SET total = u.total + EXCLUDED.total, updated_at = now();
SQL

printf "\n%-8s %-8s %-14s %-12s\n" "mode" "clients" "tps" "lat_avg_ms"
for c in $CLIENTS; do
  for mode in hot spread; do
    out=$("$PGBENCH" "$DB_URL" -n -c "$c" -j "$c" -T "$DURATION" -f "$TMP/$mode.sql" 2>&1)
    tps=$(echo "$out" | grep -E "^tps" | head -1 | sed -E 's/tps = ([0-9.]+).*/\1/')
    lat=$(echo "$out" | grep -E "latency average" | sed -E 's/.*= ([0-9.]+) ms/\1/')
    printf "%-8s %-8s %-14s %-12s\n" "$mode" "$c" "$tps" "$lat"
  done
done
echo ""
echo "Interpretation: a FLAT hot-tps with linearly-growing hot-latency vs a"
echo "linearly-scaling spread-tps is the hot-row UPSERT serialization signature."
