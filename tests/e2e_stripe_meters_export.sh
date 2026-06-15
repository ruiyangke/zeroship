#!/usr/bin/env bash
# ============================================================================
# e2e_stripe_meters_export.sh — FAITHFUL end-to-end test of the zeroship Stripe
# Billing Meters metering-export provider against REAL `https://api.stripe.com`
# (TEST mode) — NOT the in-test localhost mock (crates/control/tests/
# metering_export_test.rs).
#
# This is the [[feedback_faithful_e2e_tests]] capstone for the Stripe rail, the
# exact peer of tests/e2e_openmeter_export.sh (which gave the OpenMeter rail a
# REAL e2e). The Stripe Billing Meters rail had ONLY ever met a localhost mock;
# this harness drives the SAME hardened `metering_export` cron through the REAL
# StripeProvider/StripeClient (cyper over the wire) against a REAL Stripe Billing
# Meter the harness provisions via the API:
#
#   POST /v1/billing/meters            → provision a real meter (mtr_…)
#     ─► seed app+creator+usage in a dedicated zeroship PG
#     ─► metering_export::tick_at
#         report_usage   → POST /v1/billing/meter_events   ─► api.stripe.com
#         reported_total → GET  /v1/billing/meters/<id>/event_summaries
#     ─► poll the REAL Stripe aggregate until it converges (async aggregation)
#
# FAITHFUL by construction — NOTHING under test is stubbed:
#   * REAL Stripe Billing Meters (meter_events ingest → async aggregation →
#     event_summaries readback), the meter provisioned by THIS harness.
#   * REAL zeroship StripeProvider/StripeClient (cyper) → https://api.stripe.com.
#   * REAL dedicated zeroship Postgres (zeroship_stripe_meters_e2e on :5440) +
#     the full Liquibase changelog (the cron reads/writes usage_aggregates +
#     metering_exports).
#
# DO NO HARM: dedicated DB `zeroship_stripe_meters_e2e` on :5440 — NEVER the real
# `zeroship` DB, nor zeroship_billing_test / zeroship_metering_load /
# zeroship_invoice_demo / zeroship_stripe_e2e. Self-managed DB (re)create + drop
# on exit. Skips CLEANLY (exit 0) when prereqs are absent (no PG :5440, no psql,
# no docker for Liquibase, no keys, no control binary).
#
# Usage:
#   source /home/ruiyang/.config/zeroship-stripe-test.env   # sets the TEST keys
#   ./tests/e2e_stripe_meters_export.sh
#   STRICT=1 ./tests/e2e_stripe_meters_export.sh   # documented divergences = hard fail
#   KEEP_DB=1 ./tests/e2e_stripe_meters_export.sh  # leave the DB up for inspection
#
# SECRETS: reads $STRIPE_TEST_SECRET_KEY from the env ONLY. NEVER prints/writes/
# commits any sk_/pk_/whsec_ value (the meter id mtr_… IS printed — it is a
# non-secret resource id, like a cus_/in_).
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
STRICT="${STRICT:-0}"
KEEP_DB="${KEEP_DB:-0}"

PASS=0; FAIL=0; DIVERGENCE=0
pass()  { PASS=$((PASS+1));  echo "  ✓ $1"; }
fail()  { FAIL=$((FAIL+1));  echo "  ✗ $1"; }
# A real-Stripe divergence from the mock's assumptions, or a genuinely
# unautomatable leg. Reported loudly; hard fail only under STRICT=1.
diverge() { DIVERGENCE=$((DIVERGENCE+1)); echo "  ⚠ REAL-API NOTE: $1"; if [ "$STRICT" = "1" ]; then FAIL=$((FAIL+1)); fi; }

echo "============================================"
echo "  zeroship E2E — Stripe Billing Meters metering export (REAL api.stripe.com, not the mock)"
echo "============================================"

# --- prereq gates: skip cleanly when anything is missing --------------------
if [ -z "${STRIPE_TEST_SECRET_KEY:-}" ]; then
  echo "  ⚠ SKIP: STRIPE_TEST_SECRET_KEY not set."
  echo "         source /home/ruiyang/.config/zeroship-stripe-test.env first."
  exit 0
fi
case "$STRIPE_TEST_SECRET_KEY" in
  sk_test_*) ;;
  *) echo "  ✗ REFUSING TO RUN: STRIPE_TEST_SECRET_KEY is not an sk_test_ key. TEST mode only."; exit 2 ;;
esac
SK="$STRIPE_TEST_SECRET_KEY"

PSQL="${ZEROSHIP_PSQL:-/nix/store/0hzvyg4lmry0cv8pgl1fw9j1rddyqqbj-postgresql-17.7/bin/psql}"
PGHOST=localhost; PGPORT=5440; PGUSER=postgres; PGPW=zeroship
DB=zeroship_stripe_meters_e2e

[ -x "$PSQL" ] || { echo "  ⚠ SKIP: psql not found at $PSQL (set ZEROSHIP_PSQL)."; exit 0; }
command -v curl  >/dev/null 2>&1 || { echo "  ⚠ SKIP: curl required."; exit 0; }
command -v node  >/dev/null 2>&1 || { echo "  ⚠ SKIP: node required (JSON extraction)."; exit 0; }
command -v cargo >/dev/null 2>&1 || { echo "  ⚠ SKIP: cargo required."; exit 0; }
if [ -f "$ROOT/ops/db-migrate.sh" ] && ! command -v docker >/dev/null 2>&1; then
  echo "  ⚠ SKIP: docker required for the Liquibase migrate step."; exit 0
fi

export PGPASSWORD="$PGPW"
if ! "$PSQL" -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -tAc "SELECT 1" >/dev/null 2>&1; then
  echo "  ⚠ SKIP: Postgres :$PGPORT unreachable."; exit 0
fi

# The harness's OWN out-of-band Stripe REST driver (the provider under test uses
# its own cyper client). NEVER echo $SK.
SAPI="https://api.stripe.com/v1"
sget()  { curl -s "$SAPI/$1" -u "$SK:"; }
spost() { curl -s -X POST "$SAPI/$1" -u "$SK:" "${@:2}"; }
jget()  { node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const o=JSON.parse(s);let v=o;for(const k of process.argv[1].split(".").filter(Boolean)){v=v==null?undefined:(k.match(/^[0-9]+$/)?v[+k]:v[k]);}console.log(v==null?"":(typeof v==="object"?JSON.stringify(v):v))}catch(e){console.log("")}})' "$1"; }

# A unique event_name per run so the per-meter aggregate (and the meter itself)
# never collides with a prior run's meter on the shared test account.
EVENT_NAME="zs_cu_e2e_$(node -e 'console.log(require("crypto").randomBytes(4).toString("hex"))')"
WORK="$(mktemp -d -t zs-e2e-meters-XXXXXX)"

cleanup() {
  echo ""
  echo "=== Cleanup ==="
  # Deactivate the provisioned meter (Stripe meters cannot be hard-deleted; the
  # supported teardown is POST /v1/billing/meters/{id}/deactivate). Best-effort.
  if [ -n "${METER_ID:-}" ]; then
    spost "billing/meters/$METER_ID/deactivate" -o /dev/null 2>/dev/null || true
    echo "  deactivated the provisioned meter $METER_ID (Stripe meters are not hard-deletable)"
  fi
  if [ "$KEEP_DB" = "1" ]; then
    echo "  KEEP_DB=1 → leaving DB $DB up for inspection"
  else
    "$PSQL" -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL || true
SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname='$DB' AND pid<>pg_backend_pid();
DROP DATABASE IF EXISTS $DB;
SQL
    echo "  dropped DB $DB (the real zeroship DB + zeroship_billing_test were NEVER touched)"
  fi
  [ -n "${WORK:-}" ] && rm -rf "$WORK"
  echo "  NOTE: Stripe TEST-mode objects (mtr_/cus_/mbe_) created by this run are harmless test artifacts."
}
trap cleanup EXIT

# ===========================================================================
echo ""
echo "=== Stage 1: provision a REAL Stripe Billing Meter via the API ==="
# ===========================================================================
# POST /v1/billing/meters — verified against docs.stripe.com/api/billing/meter/create:
#   display_name              (required)
#   event_name                (required) — what the provider posts on each event
#   default_aggregation[formula]=sum     — SUM the per-event CU values
#   value_settings[event_payload_key]=value         — read CU from payload[value]
#   customer_mapping[type]=by_id                     — map by a customer id…
#   customer_mapping[event_payload_key]=stripe_customer_id  — …read from payload[stripe_customer_id]
# These payload keys are EXACTLY what crates/control/src/stripe_client.rs
# create_meter_event posts (event_name + payload[stripe_customer_id] + payload[value]).
METER_JSON="$(spost billing/meters \
  -d "display_name=zeroship CU e2e ($EVENT_NAME)" \
  -d "event_name=$EVENT_NAME" \
  -d "default_aggregation[formula]=sum" \
  -d "value_settings[event_payload_key]=value" \
  -d "customer_mapping[type]=by_id" \
  -d "customer_mapping[event_payload_key]=stripe_customer_id")"
METER_ID="$(echo "$METER_JSON" | jget id)"
METER_STATUS="$(echo "$METER_JSON" | jget status)"
case "$METER_ID" in
  mtr_*)
    pass "provisioned REAL Stripe Billing Meter $METER_ID (event_name=$EVENT_NAME, status=$METER_STATUS, aggregation=sum)"
    ;;
  *)
    ERRMSG="$(echo "$METER_JSON" | jget error.message)"
    ERRCODE="$(echo "$METER_JSON" | jget error.code)"
    # Honest reporting, like the Connect-not-enabled finding in
    # e2e_stripe_webhooks_live.sh: if the account can't create meters, say exactly why.
    echo "  ✗ could NOT create a Stripe Billing Meter on this test account."
    echo "    error.code=$ERRCODE error.message=$ERRMSG"
    diverge "POST /v1/billing/meters was rejected — '$ERRMSG' (code=$ERRCODE). Billing Meters are standard test-mode resources; if this is a permissions/feature gate, the Stripe-Meters export rail cannot be e2e'd against this account (same class as the Connect-not-enabled finding). NOTHING under test ran."
    echo ""
    echo "  Results: $PASS passed, $FAIL failed, $DIVERGENCE real-api note(s) — meter creation BLOCKED, see above"
    [ "$STRICT" = "1" ] && exit 1 || exit 0
    ;;
esac

# Confirm the meter's payload-key wiring is what the provider posts (catch a
# silent rename the mock could never surface).
MAP_KEY="$(echo "$METER_JSON" | jget customer_mapping.event_payload_key)"
VAL_KEY="$(echo "$METER_JSON" | jget value_settings.event_payload_key)"
AGG="$(echo "$METER_JSON" | jget default_aggregation.formula)"
[ "$MAP_KEY" = "stripe_customer_id" ] && pass "meter customer_mapping.event_payload_key = stripe_customer_id (matches the provider's payload)" \
  || diverge "meter customer_mapping.event_payload_key='$MAP_KEY' but the provider posts payload[stripe_customer_id] — events would not map to a customer"
[ "$VAL_KEY" = "value" ] && pass "meter value_settings.event_payload_key = value (matches the provider's payload[value])" \
  || diverge "meter value_settings.event_payload_key='$VAL_KEY' but the provider posts payload[value] — the CU would not be read"
[ "$AGG" = "sum" ] && pass "meter default_aggregation.formula = sum (delta export relies on summation)" \
  || diverge "meter aggregation='$AGG' (expected sum) — delta export math assumes Stripe SUMs events"

# ===========================================================================
echo ""
echo "=== Stage 2: dedicated zeroship DB ($DB on :$PGPORT) + Liquibase ==="
# ===========================================================================
"$PSQL" -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL || { fail "could not (re)create $DB"; exit 1; }
SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname='$DB' AND pid<>pg_backend_pid();
DROP DATABASE IF EXISTS $DB;
CREATE DATABASE $DB;
ALTER DATABASE $DB SET search_path = zeroship, public;
SQL
pass "(re)created dedicated DB $DB on :$PGPORT (NOT the real zeroship DB / billing_test / others)"

MIG_LOG="$WORK/liquibase.log"
if ZEROSHIP_DB_JDBC="jdbc:postgresql://$PGHOST:$PGPORT/$DB" "$ROOT/ops/db-migrate.sh" update > "$MIG_LOG" 2>&1; then
  pass "Liquibase changelog applied cleanly (incl. 0043 metering_exports)"
else
  fail "Liquibase migrate FAILED (see $MIG_LOG)"; tail -20 "$MIG_LOG"; exit 1
fi

DBURL="postgres://$PGUSER:$PGPW@$PGHOST:$PGPORT/$DB"

# ===========================================================================
echo ""
echo "=== Stage 3: drive the REAL export path (cargo test against REAL Stripe + PG) ==="
# ===========================================================================
# The #[ignore]'d live tests are gated on CONTROL_TEST_DB + STRIPE_LIVE_SECRET_KEY
# + STRIPE_LIVE_METER_ID (+ STRIPE_LIVE_METER_EVENT_NAME). They run the SAME
# hardened metering_export cron through the real cyper StripeClient over the wire
# to api.stripe.com, then POLL the LIVE event_summaries aggregate for convergence
# (Stripe aggregates meter events asynchronously — the divergence from the mock).
TEST_LOG="$WORK/cargo-test.log"
# Secret key passed via the ENV the test reads (STRIPE_LIVE_SECRET_KEY), never on argv.
if CONTROL_TEST_DB="$DBURL" \
   STRIPE_LIVE_SECRET_KEY="$SK" \
   STRIPE_LIVE_METER_ID="$METER_ID" \
   STRIPE_LIVE_METER_EVENT_NAME="$EVENT_NAME" \
   cargo test -p zeroship-control --test metering_export_stripe_live_test -- --ignored --nocapture --test-threads=1 \
   > "$TEST_LOG" 2>&1; then
  # Confirm tests actually RAN (not silently skipped). On the skip path the
  # gating returns green with 0 effective assertions; require a real ok.[1-9].
  if grep -qE "test result: ok\. [1-9]" "$TEST_LOG"; then
    pass "live export cargo tests PASSED against REAL Stripe (meter_events accepted + aggregate reconciled + C1 future-reject confirmed)"
    grep -E "running [0-9]+ test|test result:|\[live\]" "$TEST_LOG" | sed 's/^/    /'
  else
    fail "cargo test reported ok but ran 0 live tests (gating env not honoured?)"; tail -40 "$TEST_LOG"
  fi
else
  fail "live export cargo tests FAILED against REAL Stripe (see below — a REAL-API divergence or a provider bug)"
  echo "    --- tail of the cargo-test log ---"
  tail -50 "$TEST_LOG" | sed 's/^/    /'
fi

# ===========================================================================
echo ""
echo "============================================"
echo "  Results: $PASS passed, $FAIL failed, $DIVERGENCE real-api note(s)"
echo "  Real Stripe ids this run: meter=$METER_ID  event_name=$EVENT_NAME"
echo "============================================"
[ $FAIL -eq 0 ] && exit 0 || exit 1
