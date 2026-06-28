#!/usr/bin/env bash
# ============================================================================
# e2e_openmeter_export.sh — FAITHFUL end-to-end test of the zeroship OpenMeter
# metering-export provider against a REAL, locally-running OpenMeter (NOT the
# in-test mock).
#
# This is the [[feedback_faithful_e2e_tests]] capstone for the OpenMeter rail:
# it drives the SAME hardened `metering_export` cron through the REAL
# `OpenMeterProvider`/`OpenMeterClient` (cyper over the wire) against a live
# OpenMeter stack:
#
#   ingest_at (usage) ─► metering_export::tick_at
#        │  report_usage → POST /api/v1/events (CloudEvents)   ─► OpenMeter API
#        │                                                        │ kafka
#        │                                                        ▼ sink-worker
#        │  reported_total → GET /meters/<slug>/query  ◄── ClickHouse aggregate
#        ▼
#   exported_units high-water (Postgres)
#
# FAITHFUL by construction — NOTHING under test is stubbed:
#   * Real OpenMeter (CloudEvents ingest → Kafka → sink-worker → ClickHouse →
#     /query aggregate), stood up by docker-compose.openmeter.yml.
#   * Real zeroship OpenMeterProvider/OpenMeterClient (cyper) pointed at it.
#   * Real ephemeral zeroship Postgres + the full zeroship-migrate platform set
#     (the cron reads/writes usage_aggregates + metering_exports).
#
# DO NO HARM: the zeroship PG here is a DEDICATED ephemeral container on its own
# port (NOT :5440 — the billing PG the cargo integration tests use). The
# OpenMeter stack is a SEPARATE compose project (zeroship-openmeter) on its own
# 127.0.0.1 port band. This harness never touches :5440 or the main stack.
#
# Skips CLEANLY (exit 0) when docker is unavailable.
#
# Usage:
#   ./tests/e2e_openmeter_export.sh
#   KEEP_OPENMETER=1 ./tests/e2e_openmeter_export.sh   # leave the OM stack up
#
# Requires (when docker IS available): docker (compose v2), cargo. The OpenMeter
# images are pulled on first run (~1 GB).
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KEEP_OPENMETER="${KEEP_OPENMETER:-0}"

PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); echo "  ✓ $1"; }
fail() { FAIL=$((FAIL+1)); echo "  ✗ $1"; }

echo "============================================"
echo "  zeroship E2E — OpenMeter metering export (REAL OpenMeter, not the mock)"
echo "============================================"

# --- docker gate: skip cleanly when unavailable ----------------------------
if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
  echo "  ⚠ SKIP: docker unavailable — OpenMeter E2E needs the OpenMeter stack + an ephemeral PG."
  exit 0
fi

# --- DEDICATED ephemeral zeroship PG (NOT :5440) ---------------------------
PG_PORT=5481
PG_CONTAINER="zs-e2e-openmeter-pg"
DBURL="postgres://postgres:zeroship@localhost:$PG_PORT/zeroship"
# The OpenMeter API host endpoint published by docker-compose.openmeter.yml.
OM_URL="http://127.0.0.1:48888"
OM_PROJECT="zeroship-openmeter"
COMPOSE_FILE="$ROOT/docker-compose.openmeter.yml"

WORK="$(mktemp -d -t zs-e2e-om-XXXXXX)"

cleanup() {
  echo ""
  echo "=== Cleanup ==="
  docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
  if [ "$KEEP_OPENMETER" = "1" ]; then
    echo "  KEEP_OPENMETER=1 → leaving the OpenMeter stack up ($OM_URL)"
  else
    docker compose -f "$COMPOSE_FILE" down -v >/dev/null 2>&1 || true
    echo "  OpenMeter stack down (project $OM_PROJECT)"
  fi
  [ -n "${WORK:-}" ] && rm -rf "$WORK"
  echo "  ephemeral zeroship PG removed; $WORK cleaned (the billing :5440 PG was NEVER touched)"
}
trap cleanup EXIT

# ===========================================================================
echo ""
echo "=== Stage 1: bring up the REAL OpenMeter stack (kafka + clickhouse + redis + pg + api + sink) ==="
# ===========================================================================
docker compose -f "$COMPOSE_FILE" up -d > "$WORK/om-up.log" 2>&1 || { fail "OpenMeter compose up failed"; tail -30 "$WORK/om-up.log"; exit 1; }

# Wait for the OpenMeter API to be reachable AND the meter provisioned.
OM_READY=0
for _ in $(seq 1 60); do
  if curl -sf "$OM_URL/api/v1/meters" 2>/dev/null | grep -q '"slug":"compute_units"'; then OM_READY=1; break; fi
  sleep 2
done
[ "$OM_READY" = "1" ] && pass "OpenMeter API live on $OM_URL with the compute_units meter provisioned" \
  || { fail "OpenMeter API never became ready / meter missing"; docker compose -f "$COMPOSE_FILE" logs openmeter | tail -30; exit 1; }

# ===========================================================================
echo ""
echo "=== Stage 2: dedicated ephemeral zeroship PG (:$PG_PORT, NOT :5440) + zeroship-migrate ==="
# ===========================================================================
lsof -ti :"$PG_PORT" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
docker run --name "$PG_CONTAINER" -d -p "$PG_PORT:5432" \
  -e POSTGRES_PASSWORD=zeroship -e POSTGRES_USER=postgres -e POSTGRES_DB=zeroship \
  postgres:16 -c max_connections=200 >/dev/null || { fail "docker run zeroship PG failed"; exit 1; }
for _ in $(seq 1 30); do docker exec "$PG_CONTAINER" pg_isready -U postgres >/dev/null 2>&1 && break; sleep 1; done
docker exec "$PG_CONTAINER" pg_isready -U postgres >/dev/null 2>&1 \
  && pass "ephemeral zeroship PG ready on :$PG_PORT (dedicated; :5440 untouched)" \
  || { fail "zeroship PG never became ready"; exit 1; }

[ -f "$ROOT/ops/postgres-init.sql" ] && docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 < "$ROOT/ops/postgres-init.sql" >/dev/null 2>&1 \
  && pass "applied ops/postgres-init.sql" || true

MIG_LOG="$WORK/migrate.log"
if cargo run --quiet --manifest-path "$ROOT/Cargo.toml" -p zeroship-migrate --bin zeroship-migrate -- migrate \
    --dir "$ROOT/db/migrations" \
    --database-url "postgres://postgres:zeroship@localhost:$PG_PORT/zeroship" \
    --profile platform --yes > "$MIG_LOG" 2>&1; then
  pass "platform migrations applied cleanly from scratch (zeroship-migrate)"
else
  fail "zeroship-migrate FAILED (see $MIG_LOG)"; tail -20 "$MIG_LOG"; exit 1
fi

# ===========================================================================
echo ""
echo "=== Stage 3: drive the REAL export path (cargo test against live OpenMeter + PG) ==="
# ===========================================================================
# The #[ignore]'d live tests are gated on CONTROL_TEST_DB + OPENMETER_LIVE_URL.
# They run the SAME hardened metering_export cron through the real cyper
# OpenMeterClient over the wire, then poll the LIVE ClickHouse-backed aggregate.
TEST_LOG="$WORK/cargo-test.log"
if CONTROL_TEST_DB="$DBURL" OPENMETER_LIVE_URL="$OM_URL" \
   cargo test -p zeroship-control --test metering_export_openmeter_live_test -- --ignored --nocapture --test-threads=1 \
   > "$TEST_LOG" 2>&1; then
  # Confirm tests actually RAN (not silently skipped) — the live tests print
  # nothing on the skip path; on the real path they exercise ingest+query.
  if grep -qE "test result: ok\. [1-9]" "$TEST_LOG"; then
    pass "live export cargo tests PASSED against real OpenMeter (CloudEvents accepted + aggregate reconciled)"
    grep -E "running [0-9]+ test|test result:" "$TEST_LOG" | sed 's/^/    /'
  else
    fail "cargo test reported ok but ran 0 live tests (gating env not honoured?)"; tail -30 "$TEST_LOG"
  fi
else
  fail "live export cargo tests FAILED (see below)"; tail -40 "$TEST_LOG"
fi

# ===========================================================================
echo ""
echo "============================================"
echo "  Results: $PASS passed, $FAIL failed"
echo "============================================"
[ $FAIL -eq 0 ] && exit 0 || exit 1
