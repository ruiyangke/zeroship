#!/usr/bin/env bash
# ============================================================================
# create_demo_invoices.sh — create 3–4 CLEAN, PAID, real Stripe TEST invoices
# that DEMONSTRATE the compute-unit (CU) + per-metric usage breakdown the billing
# rail now renders onto the Stripe-hosted invoice (commit d71f6f6c). The operator
# opens these in the Stripe dashboard to SEE the CU description + the
# `metadata.compute_units` / packed per-metric `usage` on each line.
#
# This is an OPERATOR DEMO, not a CI gate. It REUSES the proven machinery in
# `tests/e2e_stripe_billing.sh` (boot a REAL `zeroship-control` against REAL
# https://api.stripe.com with the operator's TEST key; seed apps + usage +
# creators + Stripe customers; drive the REAL `bill_creator` reconcile to
# create_invoice_item + create_invoice + finalize on REAL Stripe), and ADDS:
#
#   * FOUR distinct apps/creators with DIFFERENT usage profiles so the CU varies:
#       (1) light       — modest requests/cpu/egress on a small-base Starter plan.
#       (2) heavy       — huge cpu_us + egress_bytes + db ops → high CU.
#       (3) multi       — 13 metrics → a rich per-metric breakdown + packed usage.
#       (4) prorated    — a mid-period Starter→Pro change → TWO segments → TWO
#                         invoice lines, each with ITS OWN CU breakdown.
#   * After the reconcile finalizes each invoice, PAY it on REAL Stripe via
#     `POST /v1/invoices/{in}/pay` against the customer's saved test card, so it
#     shows as a CLEAN, PAID, pristine invoice in the dashboard. NO refund, NO
#     dispute, NO void/delete — the invoices are LEFT in the test account.
#   * Fetch each invoice + its lines BACK via the API and report, per invoice:
#       the `in_…` id, the dashboard URL, the total, and the real line
#       `description` (carrying "compute units") + `metadata.compute_units` /
#       packed `usage` — proving the CU rendered on REAL Stripe. Confirms the
#       Stripe amount == our computed charge (CU is descriptive; amount unchanged).
#
# At HEAD (fefd546c, includes d71f6f6c) `create_invoice` sends
# `pending_invoice_items_behavior=include` (stripe_client.rs), so the finalized
# invoice carries the swept usage line at its real total — the D1 "$0 invoice"
# divergence the older harness flagged is FIXED here, which is what lets the pay
# leg settle a real, non-$0, CLEAN invoice.
#
# DO NO HARM: a DEDICATED DB `zeroship_invoice_demo` on :5440 — never touches the
# real `zeroship` DB, nor `zeroship_billing_test` / `zeroship_metering_load` /
# `zeroship_stripe_e2e`. Boots control on a dedicated port. Cleans up the control
# process + the local DB on exit (the DB is dropped; the Stripe invoices are
# LEFT for the operator to view).
#
# Usage:
#   source /home/ruiyang/.config/zeroship-stripe-test.env   # sets the TEST keys
#   ./tests/create_demo_invoices.sh
#
# SECRETS: reads $STRIPE_TEST_SECRET_KEY from the env ONLY. NEVER prints it,
# NEVER writes it to disk, NEVER bakes it into any artifact.
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release"

PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); echo "  ✓ $1"; }
fail() { FAIL=$((FAIL+1)); echo "  ✗ $1"; }
info() { echo "    $1"; }

echo "============================================"
echo "  zeroship — create CLEAN demo invoices (REAL Stripe TEST mode)"
echo "  Goal: 4 paid invoices showing the CU + per-metric breakdown"
echo "============================================"

# --- prereq gates -----------------------------------------------------------
PSQL="${ZEROSHIP_PSQL:-/nix/store/0hzvyg4lmry0cv8pgl1fw9j1rddyqqbj-postgresql-17.7/bin/psql}"
PGHOST=localhost; PGPORT=5440; PGUSER=postgres; PGPW=zeroship
DB=zeroship_invoice_demo

if [ -z "${STRIPE_TEST_SECRET_KEY:-}" ]; then
  echo "  ⚠ ABORT: STRIPE_TEST_SECRET_KEY not set. source /home/ruiyang/.config/zeroship-stripe-test.env first."
  exit 2
fi
case "$STRIPE_TEST_SECRET_KEY" in
  sk_test_*) ;;
  *) echo "  ✗ REFUSING TO RUN: STRIPE_TEST_SECRET_KEY is not an sk_test_ key — TEST mode only."; exit 2 ;;
esac
[ -x "$PSQL" ] || { echo "  ⚠ ABORT: psql not found at $PSQL (set ZEROSHIP_PSQL)."; exit 2; }
command -v node    >/dev/null 2>&1 || { echo "  ⚠ ABORT: node required."; exit 2; }
command -v curl    >/dev/null 2>&1 || { echo "  ⚠ ABORT: curl required."; exit 2; }
command -v openssl >/dev/null 2>&1 || { echo "  ⚠ ABORT: openssl required."; exit 2; }
command -v docker  >/dev/null 2>&1 || { echo "  ⚠ ABORT: docker required."; exit 2; }
[ -x "$BIN/zeroship-control" ] || { echo "  ⚠ ABORT: missing $BIN/zeroship-control — run: cargo build --release -p zeroship-control"; exit 2; }

export PGPASSWORD="$PGPW"
psql_db() { "$PSQL" -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d "$DB" "$@"; }
if ! "$PSQL" -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -tAc "SELECT 1" >/dev/null 2>&1; then
  echo "  ⚠ ABORT: Postgres :$PGPORT unreachable."; exit 2
fi

SK="$STRIPE_TEST_SECRET_KEY"
SAPI="https://api.stripe.com/v1"
# Stripe REST helpers — the demo's OWN out-of-band driver (control uses its own
# cyper client). NEVER echo $SK.
sget()  { curl -s "$SAPI/$1" -u "$SK:"; }
spost() { curl -s -X POST "$SAPI/$1" -u "$SK:" "${@:2}"; }
jget()  { node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{try{const o=JSON.parse(s);let v=o;for(const k of process.argv[1].split('.').filter(Boolean)){v=v==null?undefined:(k.match(/^[0-9]+$/)?v[+k]:v[k]);}console.log(v==null?'':v)}catch(e){console.log('')}})" "$1"; }

CONTROL_PORT=9182
CONTROL_URL="http://localhost:$CONTROL_PORT"
WEBHOOK_SECRET="whsec_demo_$(openssl rand -hex 16)"   # throwaway, per-run
WORK="$(mktemp -d -t zs-demo-inv-XXXXXX)"
mkdir -p "$WORK/blobs"
PIDFILE="$WORK/pids"; : > "$PIDFILE"

cleanup() {
  echo ""
  echo "=== Cleanup ==="
  if [ -f "$PIDFILE" ]; then
    while read -r pid; do [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; done < "$PIDFILE"
  fi
  wait 2>/dev/null || true
  # Drop the local demo DB (do NOT touch the Stripe invoices — operator views them).
  "$PSQL" -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -tAc \
    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname='$DB' AND pid<>pg_backend_pid(); DROP DATABASE IF EXISTS $DB;" >/dev/null 2>&1 || true
  [ -n "${WORK:-}" ] && rm -rf "$WORK"
  echo "  control down; local DB $DB dropped; $WORK cleaned."
  echo "  The REAL Stripe TEST invoices are LEFT in the account for you to view (see links above)."
}
trap cleanup EXIT

lsof -ti :"$CONTROL_PORT" 2>/dev/null | xargs -r kill -9 2>/dev/null || true

# ===========================================================================
echo ""
echo "=== Stage 1: dedicated DB ($DB) + full zeroship-migrate platform set + control at REAL Stripe ==="
# ===========================================================================
"$PSQL" -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL || { fail "could not (re)create $DB"; exit 1; }
SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname='$DB' AND pid<>pg_backend_pid();
DROP DATABASE IF EXISTS $DB;
CREATE DATABASE $DB;
ALTER DATABASE $DB SET search_path = zeroship, public;
SQL
pass "(re)created dedicated DB $DB on :$PGPORT (real zeroship + the other demo DBs untouched)"

MIG_LOG="$WORK/migrate.log"
if ZEROSHIP_MIGRATE_DSN="postgres://$PGUSER:$PGPW@$PGHOST:$PGPORT/$DB" "$ROOT/ops/db-migrate.sh" migrate --yes > "$MIG_LOG" 2>&1; then
  pass "zeroship-migrate platform set applied to $DB (plans, metric_weights, pricing_config, invoicing, proration)"
else
  fail "zeroship-migrate FAILED (see $MIG_LOG)"; tail -20 "$MIG_LOG"; exit 1
fi

DBURL="postgres://$PGUSER:$PGPW@$PGHOST:$PGPORT/$DB"
openssl genpkey -algorithm ed25519 -out "$WORK/signing-key.pem" 2>/dev/null
chmod 600 "$WORK/signing-key.pem"

STRIPE_SECRET_KEY="$SK" STRIPE_WEBHOOK_SECRET="$WEBHOOK_SECRET" \
"$BIN/zeroship-control" --port "$CONTROL_PORT" --db "$DBURL" \
  --blob-store "$WORK/blobs" --signing-key-file "$WORK/signing-key.pem" \
  --stripe-base-url "https://api.stripe.com" \
  --dev-insecure > "$WORK/control.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "$CONTROL_URL/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "$CONTROL_URL/health" >/dev/null 2>&1 \
  && pass "control healthy (stripe-base-url → REAL api.stripe.com)" \
  || { fail "control unhealthy"; tail -30 "$WORK/control.log"; exit 1; }

# ===========================================================================
echo ""
echo "=== Stage 2: seed the FOUR demo plans ==="
# ===========================================================================
# FX is the seeded global default (30,000,000 pico-cents/CU = $0.00003/CU); these
# plans inherit it (fx_pico_cents_per_unit = NULL). included_units = 0 so the full
# CU bills (the breakdown is the point). Base fees vary so the invoices read like
# real plan invoices (base + usage overage).
RTL='{"cpu_limit_ms":5000,"wall_timeout_ms":30000,"heap_limit_mb":256}'
psql_db -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL || { fail "seed plans failed"; exit 1; }
INSERT INTO zeroship.plans (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, spend_limit_default_cents, runtime_limits_json) VALUES
  ('pln_demo_starter', 'Starter', 99,  0, NULL, 5000000,  '$RTL'::jsonb),
  ('pln_demo_pro',     'Pro',     200, 0, NULL, 50000000, '$RTL'::jsonb),
  ('pln_demo_scale',   'Scale',   0,   0, NULL, 100000000,'$RTL'::jsonb)
ON CONFLICT (id) DO NOTHING;
SQL
pass "seeded plans: Starter (\$0.99 base), Pro (\$2.00 base), Scale (\$0 base) — all inherit the global FX"

# The CLOSED period the reconcile will bill: the previous calendar month relative
# to NOW. Seed usage + plan-change events keyed to THAT period (first-of-month).
NOW_UNIX="$(date +%s)"
PERIOD_START="$(node -e '
const now=new Date(Number(process.argv[1])*1000);
let y=now.getUTCFullYear(), m=now.getUTCMonth();
if(m===0){y-=1;m=11;}else{m-=1;}
process.stdout.write(String(Math.floor(Date.UTC(y,m,1,0,0,0)/1000)));
' "$NOW_UNIX")"
PERIOD_LABEL="$(node -e 'const d=new Date(Number(process.argv[1])*1000);process.stdout.write(`${d.getUTCFullYear()}-${String(d.getUTCMonth()+1).padStart(2,"0")}`)' "$PERIOD_START")"
# The 15th of the closed period, 00:00 UTC — the prorated app's mid-month change instant.
CHANGE_AT="$(node -e '
const d=new Date(Number(process.argv[1])*1000);
process.stdout.write(new Date(Date.UTC(d.getUTCFullYear(),d.getUTCMonth(),15,0,0,0)).toISOString());
' "$PERIOD_START")"
info "closed billing period = $PERIOD_LABEL (period_start unix=$PERIOD_START); prorated change_at=$CHANGE_AT"

# ===========================================================================
echo ""
echo "=== Stage 3: seed FOUR creators + apps + Stripe customers + DIFFERENT usage ==="
# ===========================================================================
# seed_creator <plan_id> creates a creator + a Stripe customer with a saved test
# card + an owned app on the given plan, and prints "CREATOR APP CUS".
seed_creator() {
  local plan_id="$1" label="$2"
  local creator app cus pm
  creator="$(node -e 'console.log(require("crypto").randomUUID())')"
  app="$(node -e 'console.log(require("crypto").randomUUID())')"
  cus="$(spost customers -d "email=demo-$label-$creator@zeroship.test" -d "name=Demo $label creator" -d "metadata[creator_id]=$creator" | jget id)"
  case "$cus" in cus_*) ;; *) echo "FAILCUS"; return 1;; esac
  pm="$(spost payment_methods/pm_card_visa/attach -d "customer=$cus" | jget id)"
  case "$pm" in pm_*) ;; *) echo "FAILPM"; return 1;; esac
  spost "customers/$cus" -d "invoice_settings[default_payment_method]=$pm" -o /dev/null
  psql_db -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL || { echo "FAILDB"; return 1; }
INSERT INTO zeroship.users (id, email, name, email_verified_at)
VALUES ('$creator', 'demo-$label-$creator@zeroship.test'::citext, 'Demo $label creator', NOW());
INSERT INTO zeroship.apps (id, name, plan_id, api_key, api_key_hash)
VALUES ('$app', 'demo-$label-app', '$plan_id', '$app', '');
INSERT INTO zeroship.app_members (app_id, user_id, role) VALUES ('$app', '$creator', 'owner');
INSERT INTO zeroship.creator_billing (creator_id) VALUES ('$creator') ON CONFLICT DO NOTHING;
INSERT INTO zeroship.billing_customer_refs (creator_id, provider, external_id)
VALUES ('$creator', 'stripe', '$cus')
ON CONFLICT (creator_id, provider) DO UPDATE SET external_id = EXCLUDED.external_id;
SQL
  echo "$creator $app $cus"
}

# usage_row <app> <metric> <total> — seed one cumulative usage_aggregates bucket
# for the closed period.
usage_row() {
  psql_db -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL || return 1
INSERT INTO zeroship.usage_aggregates (app_id, period, metric, total, updated_at)
VALUES ('$1', to_timestamp($PERIOD_START)::date, '$2', $3, NOW())
ON CONFLICT (app_id, period, metric) DO UPDATE SET total = EXCLUDED.total;
SQL
}

# ---- (1) LIGHT app: Starter plan, modest usage -----------------------------
read -r L_CREATOR L_APP L_CUS <<<"$(seed_creator pln_demo_starter light)"
case "$L_CUS" in cus_*) pass "light: creator+app+customer ($L_CUS) on Starter";; *) fail "light seed failed ($L_CREATOR $L_APP $L_CUS)"; exit 1;; esac
usage_row "$L_APP" requests     120000
usage_row "$L_APP" cpu_us       18000000
usage_row "$L_APP" egress_bytes 9000000
info "light usage: requests=120k cpu_us=18M egress_bytes=9MB → ~147,000 CU (+\$0.99 base)"

# ---- (2) HEAVY app: Scale plan, huge usage ---------------------------------
read -r H_CREATOR H_APP H_CUS <<<"$(seed_creator pln_demo_scale heavy)"
case "$H_CUS" in cus_*) pass "heavy: creator+app+customer ($H_CUS) on Scale";; *) fail "heavy seed failed"; exit 1;; esac
usage_row "$H_APP" requests     4200000
usage_row "$H_APP" cpu_us       1800000000
usage_row "$H_APP" egress_bytes 9500000000
usage_row "$H_APP" db_reads     12000000
usage_row "$H_APP" db_writes    3400000
info "heavy usage: requests=4.2M cpu=1.8Gus egress=9.5GB db_reads=12M db_writes=3.4M → ~34.3M CU"

# ---- (3) MULTI app: Pro plan, 13 metrics -----------------------------------
read -r M_CREATOR M_APP M_CUS <<<"$(seed_creator pln_demo_pro multi)"
case "$M_CUS" in cus_*) pass "multi: creator+app+customer ($M_CUS) on Pro";; *) fail "multi seed failed"; exit 1;; esac
usage_row "$M_APP" requests             650000
usage_row "$M_APP" cpu_us               240000000
usage_row "$M_APP" wall_us              900000000
usage_row "$M_APP" ingress_bytes        4000000000
usage_row "$M_APP" egress_bytes         2100000000
usage_row "$M_APP" db_reads             5200000
usage_row "$M_APP" db_writes            1350000
usage_row "$M_APP" db_rows_written      18000000
usage_row "$M_APP" kv_reads             3300000
usage_row "$M_APP" kv_writes            780000
usage_row "$M_APP" storage_ops          420000
usage_row "$M_APP" storage_bytes        6400000000
usage_row "$M_APP" storage_egress_bytes 1900000000
info "multi usage: 13 metrics → ~25.7M CU (+\$2.00 base) — a rich per-metric breakdown"

# ---- (4) PRORATED app: Starter→Pro mid-period → TWO segments ---------------
# The app's CURRENT plan is Pro (the tail). A plan_change_events row (from
# Starter → to Pro at day 15) splits the period into seg0 (Starter, days 1–14)
# and seg1 (Pro, days 15–end). usage_at_change is the CUMULATIVE snapshot at the
# change instant = seg0's end / seg1's start; usage_aggregates is the PERIOD-END
# cumulative total. seg0 delta = snapshot − 0; seg1 delta = period_end − snapshot.
read -r P_CREATOR P_APP P_CUS <<<"$(seed_creator pln_demo_pro prorated)"
case "$P_CUS" in cus_*) pass "prorated: creator+app+customer ($P_CUS), current plan Pro";; *) fail "prorated seed failed"; exit 1;; esac
# Segment-0 (Starter) usage = the cumulative snapshot at the change.
P_S0_REQ=300000;    P_S0_CPU=90000000;   P_S0_EGR=800000000
# Period-end cumulative = seg0 + seg1 deltas (seg1 adds db_reads as a new metric).
P_END_REQ=$((P_S0_REQ + 520000))
P_END_CPU=$((P_S0_CPU + 160000000))
P_END_EGR=$((P_S0_EGR + 1400000000))
P_END_DBR=2100000
usage_row "$P_APP" requests     "$P_END_REQ"
usage_row "$P_APP" cpu_us       "$P_END_CPU"
usage_row "$P_APP" egress_bytes "$P_END_EGR"
usage_row "$P_APP" db_reads     "$P_END_DBR"
PCE_ID="pce_$(openssl rand -hex 12)"
psql_db -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL || { fail "prorated plan-change seed failed"; exit 1; }
INSERT INTO zeroship.plan_change_events (id, app_id, period, from_plan_id, to_plan_id, effective_at, usage_at_change)
VALUES ('$PCE_ID', '$P_APP', to_timestamp($PERIOD_START)::date, 'pln_demo_starter', 'pln_demo_pro',
        '$CHANGE_AT'::timestamptz,
        '{"requests": $P_S0_REQ, "cpu_us": $P_S0_CPU, "egress_bytes": $P_S0_EGR}'::jsonb);
SQL
pass "prorated: Starter→Pro change at day 15 → TWO segments (seg0 Starter days1–14, seg1 Pro days15–end)"
info "prorated seg0 (Starter): req=300k cpu=90Mus egr=800MB ≈1.19M CU; seg1 (Pro): req=520k cpu=160Mus egr=1.4GB db_reads=2.1M ≈4.18M CU"

# ===========================================================================
echo ""
echo "=== Stage 4: REAL reconcile — create + finalize ALL invoices on REAL Stripe ==="
# ===========================================================================
RECON="$(curl -s -X POST "$CONTROL_URL/internal/billing/reconcile?period=$NOW_UNIX")"
info "reconcile result: $RECON"
BILLED="$(echo "$RECON" | jget billed)"
if [ "$BILLED" = "4" ]; then
  pass "reconcile billed 4 creators (4 invoices created + finalized on REAL Stripe)"
else
  fail "expected billed=4, got '$BILLED' — control.log tail:"; tail -25 "$WORK/control.log"
fi

# ===========================================================================
echo ""
echo "=== Stage 5: PAY each invoice on REAL Stripe (clean, pristine, no refund/dispute) ==="
# ===========================================================================
# Resolve each creator's finalized Stripe invoice id from billing_provider_refs,
# pay it against the saved card (POST /v1/invoices/{in}/pay), and confirm 'paid'.
# The collection_method is charge_automatically + a default PM, so /pay settles it.
declare -A INV_OF CUS_OF LABEL_OF
LABEL_OF[light]="light";  LABEL_OF[heavy]="heavy"; LABEL_OF[multi]="multi"; LABEL_OF[prorated]="prorated"
CUS_OF[light]="$L_CUS"; CUS_OF[heavy]="$H_CUS"; CUS_OF[multi]="$M_CUS"; CUS_OF[prorated]="$P_CUS"
declare -A CREATOR_OF
CREATOR_OF[light]="$L_CREATOR"; CREATOR_OF[heavy]="$H_CREATOR"; CREATOR_OF[multi]="$M_CREATOR"; CREATOR_OF[prorated]="$P_CREATOR"

resolve_inv() {
  psql_db -tA -c "SELECT bpr.external_id FROM zeroship.billing_provider_refs bpr \
    JOIN zeroship.invoices i ON i.id=bpr.invoice_id \
    WHERE i.creator_id='$1' AND bpr.provider='stripe' AND bpr.ref_kind='invoice' \
    ORDER BY bpr.created_at DESC LIMIT 1" 2>/dev/null | tr -d '[:space:]'
}

PAID_OK=0
for key in light heavy multi prorated; do
  creator="${CREATOR_OF[$key]}"
  inv="$(resolve_inv "$creator")"
  case "$inv" in
    in_*) ;;
    *) fail "$key: no finalized Stripe invoice id resolved (got '$inv')"; continue;;
  esac
  INV_OF[$key]="$inv"
  # Pay it against the saved card.
  pay_json="$(spost "invoices/$inv/pay" )"
  pstatus="$(echo "$pay_json" | jget status)"
  ppaid="$(echo "$pay_json" | jget amount_paid)"
  if [ "$pstatus" = "paid" ]; then
    pass "$key: invoice $inv PAID on REAL Stripe (amount_paid=${ppaid}c) — clean, pristine"
    PAID_OK=$((PAID_OK+1))
  else
    perr="$(echo "$pay_json" | jget error.message)"
    fail "$key: invoice $inv pay did not settle (status='$pstatus' err='$perr')"
  fi
done

# ===========================================================================
echo ""
echo "=== Stage 6: fetch each invoice + lines BACK → prove the CU breakdown rendered ==="
# ===========================================================================
# For each invoice, fetch the Stripe invoice + its lines and report the real
# total, status, hosted URL, and each line's description (with "compute units")
# + metadata.compute_units / packed usage. Confirm Stripe total == our computed
# subtotal (CU is descriptive; amount unchanged).
echo ""
echo "────────────────────────────────────────────────────────────────────────"
echo "  DEMO INVOICE SUMMARY (open these in the Stripe TEST dashboard)"
echo "────────────────────────────────────────────────────────────────────────"
for key in light heavy multi prorated; do
  inv="${INV_OF[$key]:-}"
  creator="${CREATOR_OF[$key]}"
  [ -z "$inv" ] && { echo ""; echo "  [$key] (no invoice)"; continue; }
  inv_json="$(sget "invoices/$inv")"
  total="$(echo "$inv_json" | jget total)"
  status="$(echo "$inv_json" | jget status)"
  hosted="$(echo "$inv_json" | jget hosted_invoice_url)"
  # Our computed subtotal from the local invoice row.
  our_total="$(psql_db -tA -c "SELECT total_cents FROM zeroship.invoices WHERE creator_id='$creator' AND status<>'void' ORDER BY created_at DESC LIMIT 1" 2>/dev/null | tr -d '[:space:]')"
  echo ""
  echo "  ┌─ [$key] invoice $inv"
  echo "  │   dashboard : https://dashboard.stripe.com/test/invoices/$inv"
  echo "  │   hosted    : $hosted"
  echo "  │   status    : $status      total: ${total}c (\$$(node -e "console.log((Number(process.argv[1]||0)/100).toFixed(2))" "$total"))"
  echo "  │   our charge: ${our_total}c   (Stripe total == our charge: $([ "$total" = "$our_total" ] && echo YES || echo "NO — total=$total ours=$our_total"))"
  if [ "$total" = "$our_total" ]; then pass "$key: Stripe total == our computed charge (${total}c) — CU is descriptive, amount unchanged"; else fail "$key: Stripe total ${total}c != our charge ${our_total}c"; fi
  # Lines: description + metadata per line.
  lines_json="$(sget "invoices/$inv/lines?limit=20")"
  echo "  │   lines:"
  echo "$lines_json" | node -e '
    let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{
      try{
        const o=JSON.parse(s);
        for(const l of (o.data||[])){
          const md=l.metadata||{};
          console.log("  │     • "+(l.description||"(no description)"));
          console.log("  │         amount="+(l.amount)+"c  compute_units="+(md.compute_units||"-")+
            "  billable_units="+(md.billable_units||"-")+"  segment="+(md.segment||"-"));
          if(md.usage)   console.log("  │         usage   = "+md.usage);
          if(md.usage_2) console.log("  │         usage_2 = "+md.usage_2);
          if(md.usage_truncated) console.log("  │         usage_truncated="+md.usage_truncated);
        }
      }catch(e){console.log("  │     (failed to parse lines: "+e.message+")");}
    });'
  echo "  └─"
  # Assert at least one line carries the CU description + numeric metadata.
  has_cu="$(echo "$lines_json" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const o=JSON.parse(s);const ok=(o.data||[]).some(l=>/compute units/.test(l.description||"")&&/^[0-9]+$/.test((l.metadata||{}).compute_units||""));console.log(ok?"yes":"no")}catch(e){console.log("err")}})')"
  [ "$has_cu" = "yes" ] && pass "$key: REAL Stripe line renders \"compute units\" + numeric metadata.compute_units" \
    || fail "$key: no line carried the CU description + numeric compute_units (got '$has_cu')"
done

echo ""
echo "============================================"
echo "  Results: $PASS passed, $FAIL failed   (invoices paid: $PAID_OK/4)"
echo "  The invoices above are LEFT in the Stripe TEST account — open the dashboard links to view the CU breakdown."
echo "============================================"
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
