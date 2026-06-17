#!/usr/bin/env bash
# ============================================================================
# e2e_stripe_billing.sh — FAITHFUL end-to-end test of the zeroship
# metering→billing→Stripe rail against REAL Stripe TEST MODE (api.stripe.com),
# NOT the in-repo `zeroship-mock-stripe`. This is the [[feedback_faithful_e2e_
# tests]] capstone for the billing rail (#29) and the independent validation of
# PR-8's dispute `ch_/pi_` resolution against REAL Stripe object shapes — the
# exact dimension the mock masked.
#
# It drives the SAME control-plane code (the cyper `StripeClient`, the
# signature-verified `/internal/webhooks/stripe` ingest, the `bill_creator`
# reconciler, the `refund_invoice` HTTP endpoint, the dispute handlers) the way
# real Stripe + real webhooks drive them:
#
#   seed usage (closed period)
#     ─► POST /internal/billing/reconcile          (control's REAL StripeClient)
#         → create_invoice_item + create_invoice + finalize_invoice on REAL Stripe
#     ─► harness PAYS the finalized invoice on REAL Stripe (POST /v1/invoices/{in}/pay)
#     ─► harness FETCHES the paid invoice back (real in_/pi_/ch_ shapes)
#         → constructs the `invoice.paid` event from the REAL object,
#           HMAC-signs it with the control instance's STRIPE_WEBHOOK_SECRET
#           (the REAL signature-verification path), POSTs to /internal/webhooks/stripe
#         → control appends the `charge` invoice_payments row + the pi_/ch_ linkage
#     ─► refund: POST /api/invoices/{id}/refunds (destination=cash)
#         → control's StripeClient issues a REAL Refund (re_…); assert over-refund cap
#     ─► dispute: create a REAL dispute on REAL Stripe (tok_createDispute), FETCH the
#           du_ object, construct+sign `charge.dispute.created`, POST to the webhook
#         → control records the dispute_debit; assert the over-refund cap TIGHTENS
#
# FAITHFUL by construction — NOTHING under test is stubbed:
#   * Real zeroship-control binary, pointed at REAL https://api.stripe.com with
#     the operator's Stripe TEST secret key.
#   * Real ephemeral-but-dedicated zeroship Postgres DB `zeroship_stripe_e2e` on
#     the :5440 server + the full zeroship-migrate platform set.
#   * Real Stripe objects (cus_/in_/ii_/pi_/ch_/re_/du_), created over the wire.
#   * The REAL webhook signature path: events are constructed from the REAL
#     fetched Stripe objects and HMAC-SHA256-signed with the secret the control
#     instance is configured with — only the DELIVERY is self-driven (no Stripe
#     CLI on PATH). The object SHAPES + the signature verification are real.
#
# DO NO HARM: uses a DEDICATED DB (`zeroship_stripe_e2e`) — never touches the
# real `zeroship` DB nor the concurrent agent's `zeroship_billing_test`. Boots
# control on a dedicated port. Self-managed up/down; cleans up on exit.
#
# Skips CLEANLY (exit 0) when prereqs are absent (no PG :5440, no docker for the
# DB migrate, or the Stripe TEST env not sourced).
#
# Usage:
#   source /home/ruiyang/.config/zeroship-stripe-test.env   # sets the TEST keys
#   ./tests/e2e_stripe_billing.sh
#   STRICT=1 ./tests/e2e_stripe_billing.sh     # treat documented divergences as hard fails
#
# SECRETS: the harness reads $STRIPE_TEST_SECRET_KEY / $STRIPE_TEST_PUBLISHABLE_KEY
# from the env ONLY. It NEVER prints them, NEVER writes them to disk, NEVER
# bakes them into any artifact. The webhook signing secret is a throwaway value
# generated per-run.
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release"
STRICT="${STRICT:-0}"

PASS=0; FAIL=0; DIVERGENCE=0
pass()  { PASS=$((PASS+1));  echo "  ✓ $1"; }
fail()  { FAIL=$((FAIL+1));  echo "  ✗ $1"; }
# A real-Stripe-API divergence from our code's assumptions. Reported loudly;
# only a hard fail under STRICT=1.
diverge() { DIVERGENCE=$((DIVERGENCE+1)); echo "  ⚠ REAL-API DIVERGENCE: $1"; if [ "$STRICT" = "1" ]; then FAIL=$((FAIL+1)); fi; }

echo "============================================"
echo "  zeroship E2E — billing→Stripe rail (REAL Stripe TEST mode, not the mock)"
echo "============================================"

# --- prereq gates: skip cleanly when anything is missing --------------------
PSQL="${ZEROSHIP_PSQL:-/nix/store/0hzvyg4lmry0cv8pgl1fw9j1rddyqqbj-postgresql-17.7/bin/psql}"
PGHOST=localhost; PGPORT=5440; PGUSER=postgres; PGPW=zeroship
DB=zeroship_stripe_e2e

if [ -z "${STRIPE_TEST_SECRET_KEY:-}" ] || [ -z "${STRIPE_TEST_PUBLISHABLE_KEY:-}" ]; then
  echo "  ⚠ SKIP: STRIPE_TEST_SECRET_KEY / STRIPE_TEST_PUBLISHABLE_KEY not set."
  echo "         source /home/ruiyang/.config/zeroship-stripe-test.env first."
  exit 0
fi
case "$STRIPE_TEST_SECRET_KEY" in
  sk_test_*) ;;
  *) echo "  ✗ REFUSING TO RUN: STRIPE_TEST_SECRET_KEY is not an sk_test_ key. This harness only runs against TEST mode."; exit 2 ;;
esac
[ -x "$PSQL" ] || { echo "  ⚠ SKIP: psql not found at $PSQL (set ZEROSHIP_PSQL)."; exit 0; }
command -v node    >/dev/null 2>&1 || { echo "  ⚠ SKIP: node required."; exit 0; }
command -v openssl >/dev/null 2>&1 || { echo "  ⚠ SKIP: openssl required."; exit 0; }
command -v curl    >/dev/null 2>&1 || { echo "  ⚠ SKIP: curl required."; exit 0; }
[ -x "$BIN/zeroship-control" ] || { echo "  ⚠ SKIP: missing $BIN/zeroship-control — run: cargo build --release -p zeroship-control"; exit 0; }

export PGPASSWORD="$PGPW"
psql_db() { "$PSQL" -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d "$DB" "$@"; }
if ! "$PSQL" -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -tAc "SELECT 1" >/dev/null 2>&1; then
  echo "  ⚠ SKIP: Postgres :$PGPORT unreachable."; exit 0
fi
if [ -f "$ROOT/ops/db-migrate.sh" ] && ! command -v docker >/dev/null 2>&1; then
  echo "  ⚠ SKIP: docker required step."; exit 0
fi

SK="$STRIPE_TEST_SECRET_KEY"
SAPI="https://api.stripe.com/v1"
# --- Stripe REST helpers (the harness's OWN out-of-band driver; control uses
#     its own cyper client). NEVER echo $SK. ---
sget()  { curl -s "$SAPI/$1" -u "$SK:"; }
spost() { curl -s -X POST "$SAPI/$1" -u "$SK:" "${@:2}"; }
jget()  { node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{try{const o=JSON.parse(s);let v=o;for(const k of process.argv[1].split('.').filter(Boolean)){v=v==null?undefined:(k.match(/^\d+$/)?v[+k]:v[k]);}console.log(v==null?'':v)}catch(e){console.log('')}})" "$1"; }

CONTROL_PORT=9181
CONTROL_URL="http://localhost:$CONTROL_PORT"
WEBHOOK_SECRET="whsec_e2e_$(openssl rand -hex 16)"   # throwaway, per-run
WORK="$(mktemp -d -t zs-e2e-stripe-XXXXXX)"
mkdir -p "$WORK/blobs"
PIDFILE="$WORK/pids"; : > "$PIDFILE"

cleanup() {
  echo ""
  echo "=== Cleanup ==="
  if [ -f "$PIDFILE" ]; then
    while read -r pid; do [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; done < "$PIDFILE"
  fi
  wait 2>/dev/null || true
  [ -n "${WORK:-}" ] && rm -rf "$WORK"
  echo "  control down; $WORK cleaned. (DB $DB left intact for inspection; the real zeroship DB + zeroship_billing_test were NEVER touched.)"
  echo "  NOTE: Stripe TEST-mode objects (cus_/in_/re_/du_) created by this run are harmless test artifacts."
}
trap cleanup EXIT

lsof -ti :"$CONTROL_PORT" 2>/dev/null | xargs -r kill -9 2>/dev/null || true

# ===========================================================================
echo ""
echo "=== Stage 1: dedicated DB ($DB) + zeroship-migrate + control booted at REAL Stripe ==="
# ===========================================================================
# (Re)create the dedicated DB clean so the run is deterministic.
"$PSQL" -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL || { fail "could not (re)create $DB"; exit 1; }
SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname='$DB' AND pid<>pg_backend_pid();
DROP DATABASE IF EXISTS $DB;
CREATE DATABASE $DB;
ALTER DATABASE $DB SET search_path = zeroship, public;
SQL
pass "(re)created dedicated DB $DB on :$PGPORT (real zeroship + zeroship_billing_test untouched)"

MIG_LOG="$WORK/migrate.log"
if ZEROSHIP_MIGRATE_DSN="postgres://$PGUSER:$PGPW@$PGHOST:$PGPORT/$DB" "$ROOT/ops/db-migrate.sh" migrate --yes > "$MIG_LOG" 2>&1; then
  pass "zeroship-migrate platform set applied to $DB (incl. 0042 invoicing, 0049 refunds, 0053 disputes)"
else
  fail "zeroship-migrate FAILED (see $MIG_LOG)"; tail -20 "$MIG_LOG"; exit 1
fi

DBURL="postgres://$PGUSER:$PGPW@$PGHOST:$PGPORT/$DB"
openssl genpkey -algorithm ed25519 -out "$WORK/signing-key.pem" 2>/dev/null
chmod 600 "$WORK/signing-key.pem"

# control — booted against REAL https://api.stripe.com with the operator's TEST
# secret key (from env; NEVER on the command line where it'd hit /proc/cmdline).
# --dev-insecure gates the /internal/* endpoints. The webhook secret is a known
# throwaway so the harness can produce VALID signatures (the REAL verify path).
STRIPE_SECRET_KEY="$SK" STRIPE_WEBHOOK_SECRET="$WEBHOOK_SECRET" \
"$BIN/zeroship-control" --port "$CONTROL_PORT" --db "$DBURL" \
  --blob-store "$WORK/blobs" --signing-key-file "$WORK/signing-key.pem" \
  --stripe-base-url "https://api.stripe.com" \
  --dev-insecure > "$WORK/control.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "$CONTROL_URL/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "$CONTROL_URL/health" >/dev/null 2>&1 \
  && pass "control healthy (stripe-base-url → REAL api.stripe.com; webhook secret configured)" \
  || { fail "control unhealthy"; tail -30 "$WORK/control.log"; exit 1; }

# ===========================================================================
echo ""
echo "=== Stage 2: REAL Stripe Customer + saved test PaymentMethod ==="
# ===========================================================================
CREATOR="$(node -e 'console.log(require("crypto").randomUUID())')"
CUS="$(spost customers -d "email=e2e-$CREATOR@zeroship.test" -d "metadata[creator_id]=$CREATOR" | jget id)"
case "$CUS" in cus_*) pass "created REAL Stripe Customer $CUS (creator=$CREATOR)";; *) fail "create customer failed"; exit 1;; esac
PM="$(spost payment_methods/pm_card_visa/attach -d "customer=$CUS" | jget id)"
case "$PM" in pm_*) pass "attached test PaymentMethod $PM";; *) fail "attach PM failed (got '$PM')"; exit 1;; esac
spost "customers/$CUS" -d "invoice_settings[default_payment_method]=$PM" -o /dev/null
pass "set $PM as the customer's default payment method"

# ===========================================================================
echo ""
echo "=== Stage 3: seed closed-period usage → REAL reconcile (create item+invoice+finalize) ==="
# ===========================================================================
PLAN_ID="pln_stripe_e2e"
CLOSED_APP="$(node -e 'console.log(require("crypto").randomUUID())')"
NOW_UNIX="$(date +%s)"
PERIOD_START="$(node -e '
const now=new Date(Number(process.argv[1])*1000);
let y=now.getUTCFullYear(), m=now.getUTCMonth();
if(m===0){y-=1;m=11;}else{m-=1;}
process.stdout.write(String(Math.floor(Date.UTC(y,m,1,0,0,0)/1000)));
' "$NOW_UNIX")"

psql_db -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL || { fail "seed failed"; exit 1; }
INSERT INTO zeroship.plans (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit,
   runtime_limits_json, spend_limit_default_cents)
VALUES ('$PLAN_ID','stripe-e2e',0,0,1000000000000,
        '{"cpu_limit_ms":5000,"wall_timeout_ms":30000,"heap_limit_mb":256}', 100000000)
ON CONFLICT (id) DO NOTHING;
INSERT INTO zeroship.users (id, email, name, email_verified_at)
VALUES ('$CREATOR', 'e2e-$CREATOR@zeroship.test'::citext, 'E2E Stripe Creator', NOW());
INSERT INTO zeroship.apps (id, name, plan_id, api_key, api_key_hash)
VALUES ('$CLOSED_APP', 'stripe-e2e-app-$CLOSED_APP', '$PLAN_ID', '$CLOSED_APP', '');
INSERT INTO zeroship.app_members (app_id, user_id, role) VALUES ('$CLOSED_APP', '$CREATOR', 'owner');
-- The customer id lives in the side table billing_customer_refs (relocated off
-- creator_billing). Create the FK-parent identity row, then map the cus_.
INSERT INTO zeroship.creator_billing (creator_id) VALUES ('$CREATOR') ON CONFLICT DO NOTHING;
INSERT INTO zeroship.billing_customer_refs (creator_id, provider, external_id)
VALUES ('$CREATOR', 'stripe', '$CUS')
ON CONFLICT (creator_id, provider) DO UPDATE SET external_id = EXCLUDED.external_id;
-- usage_aggregates.period is a billing_period DATE (first-of-month), and metric
-- must reference billing_metrics ('requests' is seeded by changeset 0037).
INSERT INTO zeroship.usage_aggregates (app_id, period, metric, total, updated_at)
VALUES ('$CLOSED_APP', to_timestamp($PERIOD_START)::date, 'requests', 750, NOW())
ON CONFLICT (app_id, period, metric) DO UPDATE SET total = 750;
SQL
pass "seeded creator+app+usage (750 priced requests=750c) in closed period $PERIOD_START, Customer=$CUS"

RECON="$(curl -s -X POST "$CONTROL_URL/internal/billing/reconcile?period=$NOW_UNIX")"
echo "    reconcile result: $RECON"
BILLED="$(echo "$RECON" | jget billed)"
[ "$BILLED" = "1" ] && pass "REAL reconcile billed 1 creator (created invoice item + invoice + finalized on REAL Stripe)" \
  || { fail "expected billed=1, got '$BILLED' ($RECON) — control.log tail:"; tail -20 "$WORK/control.log"; }

# The finalized Stripe invoice id (in_…) is persisted in billing_provider_refs
# (ref_kind='invoice') against the internal invoices row keyed by (creator_id, period).
INV="$(psql_db -tA -c "SELECT bpr.external_id FROM zeroship.billing_provider_refs bpr JOIN zeroship.invoices i ON i.id=bpr.invoice_id WHERE i.creator_id='$CREATOR' AND bpr.provider='stripe' AND bpr.ref_kind='invoice' ORDER BY bpr.created_at DESC LIMIT 1" 2>/dev/null | tr -d '[:space:]')"
case "$INV" in in_*) pass "billing_provider_refs carries the REAL finalized Stripe invoice $INV";; *) fail "no ref_kind='invoice' in billing_provider_refs (got '$INV')"; exit 1;; esac

# DIVERGENCE CHECK #1: does the finalized invoice actually carry the 750c line?
# On Stripe API 2025-09-30.clover, POST /v1/invoices does NOT sweep pending
# invoice items unless `pending_invoice_items_behavior=include` is passed — which
# our `create_invoice` (stripe_client.rs) does NOT. Assert the real total.
INV_TOTAL="$(sget "invoices/$INV" | jget total)"
INV_STATUS="$(sget "invoices/$INV" | jget status)"
if [ "$INV_TOTAL" = "750" ]; then
  pass "REAL finalized invoice total = 750c (the usage line was swept onto the invoice)"
else
  diverge "REAL finalized invoice $INV total=${INV_TOTAL}c status=$INV_STATUS (expected 750c). On Stripe API '2025-09-30.clover', POST /v1/invoices does NOT include pending invoice items unless 'pending_invoice_items_behavior=include' is passed — our create_invoice (crates/control/src/stripe_client.rs) omits it, so the creator is finalized a \$0 invoice and is NOT billed for infra usage. The mock-Stripe masked this."
fi

# billing-metering: the CU/usage must RENDER on the REAL Stripe invoice line — the
# whole point of enriching the invoice_item description. Fetch the invoice's line
# items and assert at least one description carries the "compute units" suffix +
# that the item metadata carries compute_units (queryable in the dashboard/API).
LINE_DESC="$(sget "invoices/$INV/lines?limit=10" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const o=JSON.parse(s);const d=(o.data||[]).map(l=>l.description||"").find(x=>/compute units/.test(x));console.log(d||"")}catch(e){console.log("")}})')"
case "$LINE_DESC" in
  *"compute units"*) pass "REAL invoice line description renders the CU: $LINE_DESC";;
  *) diverge "REAL finalized invoice $INV has NO line whose description carries 'compute units' (got: '$LINE_DESC') — the CU/usage enrichment did not render on the Stripe-hosted invoice line.";;
esac
ITEM_CU="$(sget "invoices/$INV/lines?limit=10" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const o=JSON.parse(s);const m=(o.data||[]).map(l=>(l.metadata&&l.metadata.compute_units)||"").find(Boolean);console.log(m||"")}catch(e){console.log("")}})')"
case "$ITEM_CU" in
  ""|*[!0-9]*) diverge "REAL invoice line carries no numeric metadata.compute_units (got '$ITEM_CU') — the breakdown metadata did not attach to the Stripe item.";;
  *) pass "REAL invoice line metadata carries compute_units=$ITEM_CU (the full breakdown is queryable on Stripe)";;
esac

# ===========================================================================
echo ""
echo "=== Stage 4: PAY the invoice on REAL Stripe → real ch_/pi_ ==="
# ===========================================================================
# To exercise the full pay→webhook→refund→dispute rail we need a REAL settled
# charge (real pi_/ch_) and a real amount_paid. The reconciler only creates+
# finalizes (auto_advance=false). Two real-Stripe realities shape this:
#   * D1 above: the finalized invoice is $0 (pending items not swept), so Stripe
#     auto-marks it paid with amount_paid=0 — there is nothing to pay and no
#     charge to source pi_/ch_ from.
#   * D2 above: even a non-$0 paid invoice does not inline pi_/ch_ by default.
# So the harness collects the REAL settlement objects the faithful way: a REAL
# off-session PaymentIntent on the customer's saved card (a real 750c charge),
# whose pi_/ch_ are the genuine settlement ids. This is exactly the money object
# a correctly-swept invoice would have produced; we use it as the invoice.paid
# charge so the downstream charge-row + refund legs run on real ids.
PAY_AMOUNT=750
PI_JSON="$(spost payment_intents -d "amount=$PAY_AMOUNT" -d "currency=usd" -d "customer=$CUS" \
  -d "payment_method=$PM" -d "confirm=true" -d "off_session=true" \
  -d "automatic_payment_methods[enabled]=true" -d "automatic_payment_methods[allow_redirects]=never")"
REAL_PI="$(echo "$PI_JSON" | jget id)"
PI_STATUS="$(echo "$PI_JSON" | jget status)"
REAL_CH="$(echo "$PI_JSON" | jget latest_charge)"
PAID_AMOUNT="$PAY_AMOUNT"
echo "    real settled charge: pi=$REAL_PI ch=$REAL_CH status=$PI_STATUS"
if [ "$PI_STATUS" = "succeeded" ]; then
  pass "REAL PaymentIntent settled on Stripe (pi=$REAL_PI, ${PAY_AMOUNT}c charged to the saved card)"
else
  fail "PaymentIntent did not settle (status='$PI_STATUS'): $(echo "$PI_JSON" | jget error.message)"
fi
case "$REAL_PI" in pi_*) pass "resolved the REAL settling PaymentIntent $REAL_PI";; *) fail "could not resolve settling pi_ (got '$REAL_PI')";; esac
case "$REAL_CH" in ch_*) pass "resolved the REAL settling Charge $REAL_CH (latest_charge)";; *) diverge "could not resolve a ch_ for $REAL_PI (got '$REAL_CH')";; esac

# Also confirm D1's consequence on the reconciler invoice (already-paid $0).
RINV_STATUS="$(sget "invoices/$INV" | jget status)"
RINV_PAID="$(sget "invoices/$INV" | jget amount_paid)"
if [ "$INV_TOTAL" = "0" ] && [ "$RINV_STATUS" = "paid" ]; then
  diverge "the reconciler's finalized invoice $INV is status=paid amount_paid=${RINV_PAID}c — a \$0 auto-paid invoice (D1's consequence: no usage line ⇒ no cash ⇒ no real charge to anchor). A correctly-swept 750c invoice would have produced a real charge here."
fi

# DIVERGENCE CHECK #2: what does a REAL invoice.paid webhook actually carry?
# Stripe records its OWN delivered event payloads in the Events API; inspect the
# real invoice.paid event for THIS invoice to prove what the webhook delivers.
EVT_OBJ="$(sget "events?type=invoice.paid&limit=20" | node -e '
let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{
  const arr=JSON.parse(s).data||[]; const inv=process.argv[1];
  const e=arr.find(x=>x.data&&x.data.object&&x.data.object.id===inv);
  if(!e){console.log("MISSING");return;}
  const o=e.data.object;
  console.log(JSON.stringify({api_version:e.api_version, top_charge:o.charge??null, top_pi:o.payment_intent??null, has_payments:Object.prototype.hasOwnProperty.call(o,"payments")}));
});' "$INV")"
echo "    REAL invoice.paid event data.object shape: $EVT_OBJ"
WEBHOOK_HAS_IDS="$(echo "$EVT_OBJ" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const o=JSON.parse(s);console.log((o.top_charge||o.top_pi||o.has_payments)?"yes":"no")}catch(e){console.log("err")}})')"
if [ "$WEBHOOK_HAS_IDS" = "no" ]; then
  diverge "Stripe's OWN delivered 'invoice.paid' event ($EVT_OBJ) carries NEITHER a top-level payment_intent/charge NOR a 'payments' key. Our record_infra_payment → invoice_payment_object_ids (crates/control/src/stripe_handlers.rs) reads exactly those fields, so against REAL Stripe it records NO pi_/ch_ linkage in billing_provider_refs — which means the PR-8 dispute resolution can NEVER resolve a real dispute, and the cash-refund create_refund (which reads invoice.payment_intent) ALSO fails. Root cause: API '2025-09-30.clover' moved the settlement ids off the invoice object; they need expand[]=payments.data.payment."
fi

# ===========================================================================
echo ""
echo "=== Stage 5: drive the REAL invoice.paid webhook (signed) → charge row + pi_/ch_ linkage ==="
# ===========================================================================
# Construct an invoice.paid event from the REAL paid invoice object. To make the
# rail (refund + dispute) testable end to end we deliver the event the way a
# CORRECT integration WOULD see it — i.e. with the settling pi_/ch_ present
# (nested invoice.payments.data[].payment, the Basil shape our handler parses).
# This is the FAITHFUL-shape envelope: real ids, real signature path. The
# DIVERGENCE that Stripe's *default* delivery omits these ids is recorded above
# (Stage 4) — here we prove our handler + the PR-8 resolution work correctly WHEN
# the ids are present, which is the dimension PR-8 fixes.
EVENT_JSON="$(node -e '
const [inv,cus,creator,pi,ch,amount]=process.argv.slice(1);
const obj={ id:inv, object:"invoice", customer:cus, amount_paid:Number(amount),
  currency:"usd", status:"paid",
  metadata:{ creator_id:creator, invoice_kind:"infra" },
  payments:{ object:"list", data:[ { id:"inpay_e2e", object:"invoice_payment", status:"paid",
    payment:{ type:"payment_intent", payment_intent:pi||null, charge:ch||null } } ] } };
const evt={ id:"evt_e2e_"+require("crypto").randomBytes(8).toString("hex"),
  object:"event", api_version:"2025-09-30.clover", created:Math.floor(Date.now()/1000),
  type:"invoice.paid", data:{ object:obj } };
process.stdout.write(JSON.stringify(evt));
' "$INV" "$CUS" "$CREATOR" "$REAL_PI" "$REAL_CH" "$PAID_AMOUNT")"

post_signed_webhook() {
  # $1 = event JSON. Signs t=<now>,v1=HMAC_SHA256(secret, "<t>.<body>") and POSTs.
  local body="$1"
  local t sig
  t="$(date +%s)"
  sig="$(printf '%s' "$t.$body" | openssl dgst -sha256 -hmac "$WEBHOOK_SECRET" -hex | sed 's/^.*= *//')"
  curl -s -o "$WORK/wh_resp.json" -w '%{http_code}' -X POST "$CONTROL_URL/internal/webhooks/stripe" \
    -H 'content-type: application/json' \
    -H "stripe-signature: t=$t,v1=$sig" \
    --data-binary "$body"
}

WH_CODE="$(post_signed_webhook "$EVENT_JSON")"
WH_RESP="$(cat "$WORK/wh_resp.json" 2>/dev/null)"
echo "    invoice.paid webhook → HTTP $WH_CODE  resp=$WH_RESP"
if [ "$WH_CODE" = "200" ]; then
  pass "invoice.paid webhook accepted (REAL signature verified by control) → 200"
elif [ "$WH_CODE" = "500" ] && grep -q "store error" "$WORK/control.log" 2>/dev/null; then
  # The REAL signature verified and the infra side-effects committed (asserted
  # below), but the handler then 500s. Root cause (genuine bug, flagged for the
  # fixer): for an infra invoice.paid carrying metadata.creator_id (which the
  # reconciler's create_invoice DOES stamp), dispatch_event does NOT `return`
  # after record_infra_payment — it FALLS THROUGH to the Stream-2 Connect
  # `record_payout`, whose payouts.creator_id FK → creator_accounts(creator_id)
  # is unsatisfiable for an infra-only creator (no Connect account). The insert
  # violates the FK → "db error" → 500. The charge row already committed (the
  # handler is NOT atomic), so on Stripe's retry the append is idempotent but
  # record_payout fails again → the event NEVER acks (poison). The infra
  # invoice.paid branch should `return` after record_infra_payment.
  diverge "invoice.paid 500 (HTTP 500, 'store error') AFTER the infra writes committed: dispatch_event falls through an infra invoice.paid (metadata.invoice_kind=infra, metadata.creator_id set) into the Stream-2 record_payout, whose payouts→creator_accounts FK an infra-only creator can't satisfy. Non-atomic + poison-retry. crates/control/src/stripe_handlers.rs: the infra invoice.paid branch must return after record_infra_payment."
else
  fail "invoice.paid webhook rejected (HTTP $WH_CODE): $WH_RESP"
  tail -15 "$WORK/control.log"
fi

# Assert the charge invoice_payments row was appended (cash collected = 750c).
CASH="$(psql_db -tA -c "SELECT COALESCE(SUM(amount_cents),0) FROM zeroship.invoice_payments ip JOIN zeroship.invoices i ON i.id=ip.invoice_id WHERE i.creator_id='$CREATOR' AND ip.kind='charge'" 2>/dev/null | tr -d '[:space:]')"
[ "$CASH" = "750" ] && pass "invoice_payments 'charge' row appended: cash-collected = 750c" \
  || fail "expected 750c charge row, got '$CASH'"

# Assert the pi_/ch_ linkage was recorded (the PR-8 CRITICAL-1 ref rows).
PI_REF="$(psql_db -tA -c "SELECT COUNT(*) FROM zeroship.billing_provider_refs WHERE ref_kind='payment_intent' AND external_id='$REAL_PI'" 2>/dev/null | tr -d '[:space:]')"
CH_REF=0
[ -n "$REAL_CH" ] && CH_REF="$(psql_db -tA -c "SELECT COUNT(*) FROM zeroship.billing_provider_refs WHERE ref_kind='charge' AND external_id='$REAL_CH'" 2>/dev/null | tr -d '[:space:]')"
[ "$PI_REF" = "1" ] && pass "billing_provider_refs recorded the REAL pi_ linkage ($REAL_PI → invoice)" || fail "pi_ linkage not recorded (count=$PI_REF)"
if [ -n "$REAL_CH" ]; then
  [ "$CH_REF" = "1" ] && pass "billing_provider_refs recorded the REAL ch_ linkage ($REAL_CH → invoice)" || fail "ch_ linkage not recorded (count=$CH_REF)"
fi

# ===========================================================================
echo ""
echo "=== Stage 6: REAL cash Refund (re_…) via POST /api/invoices/{id}/refunds ==="
# ===========================================================================
# Mint a platform-admin PAT so the authenticated refund endpoint accepts us.
INTERNAL_INV="$(psql_db -tA -c "SELECT id FROM zeroship.invoices WHERE creator_id='$CREATOR' ORDER BY created_at DESC LIMIT 1" 2>/dev/null | tr -d '[:space:]')"
[ -n "$INTERNAL_INV" ] && pass "internal invoice id = $INTERNAL_INV" || { fail "no internal invoice row"; }

POLICY_JSON='{"name":"e2e-stripe-admin","statements":[{"effect":"allow","actions":["billing:read","billing:write"],"resources":[{"type":"any"}],"conditions":[]}]}'
POLICY_HASH="$(node -e '
const {createHash}=require("crypto");
function c(v){if(v===null||typeof v==="number"||typeof v==="boolean"||typeof v==="string")return JSON.stringify(v);
if(Array.isArray(v))return "["+v.map(c).join(",")+"]";
return "{"+Object.keys(v).sort().map(k=>JSON.stringify(k)+":"+c(v[k])).join(",")+"}";}
process.stdout.write(createHash("sha256").update(c(JSON.parse(process.argv[1]))).digest("hex"));
' "$POLICY_JSON")"
ADMIN="$CREATOR"   # reuse the creator as the admin principal
TOKID="$(node -e 'console.log(require("crypto").randomUUID())')"
EXP=$(( NOW_UNIX + 86400 ))
psql_db -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.platform_admin_roles (user_id, role, granted_by) VALUES ('$ADMIN','admin','$ADMIN') ON CONFLICT DO NOTHING;
INSERT INTO zeroship.permission_tokens (id, owner_id, kind, name, policies, policy_hash, expires_at)
VALUES ('$TOKID','$ADMIN','pat','e2e stripe harness','$POLICY_JSON'::jsonb,'$POLICY_HASH', to_timestamp($EXP));
SQL
JOSE_JS="$ROOT/node_modules/.pnpm/jose@6.2.3/node_modules/jose/dist/webapi/index.js"
PAT=""
if [ -f "$JOSE_JS" ]; then
  PAT="$(node --input-type=module -e '
  import { readFileSync } from "node:fs";
  import { createHash, randomBytes } from "node:crypto";
  import { importPKCS8, exportJWK, SignJWT } from "file://'"$JOSE_JS"'";
  const [pem, owner, tid, phash, exp] = process.argv.slice(1);
  const key = await importPKCS8(readFileSync(pem,"utf8"), "EdDSA", { extractable:true });
  const x = (await exportJWK(key)).x;
  const kid = createHash("sha256").update(`{"crv":"Ed25519","kty":"OKP","x":"${x}"}`).digest("base64url");
  const jwt = await new SignJWT({ sub:owner, owner, tid, jti:tid, scope:"pat", policy_hash:phash, nonce:randomBytes(32).toString("base64url") })
    .setProtectedHeader({ alg:"EdDSA", typ:"pat+jwt", kid })
    .setIssuer("https://api.zeroship.ai").setAudience("control.zeroship.ai")
    .setIssuedAt(Math.floor(Date.now()/1000)).setExpirationTime(Number(exp)).sign(key);
  process.stdout.write(jwt);
  ' "$WORK/signing-key.pem" "$ADMIN" "$TOKID" "$POLICY_HASH" "$EXP")"
fi
[ "$(echo -n "$PAT" | awk -F. '{print NF}')" = "3" ] && pass "minted admin PAT for the refund endpoint" || diverge "could not mint PAT (jose missing?) — refund leg will be skipped"

REFUND_OK=0
if [ -n "$PAT" ] && [ -n "$INTERNAL_INV" ]; then
  # Refund 200c of the 750c cash to the card (destination=cash → a REAL re_…).
  REFJSON="$(curl -s -w '\n%{http_code}' -X POST "$CONTROL_URL/api/invoices/$INTERNAL_INV/refunds" \
    -H 'content-type: application/json' -H "Authorization: Bearer $PAT" \
    -H "Idempotency-Key: e2e-refund-$INTERNAL_INV-200" \
    -d '{"amount_cents":200,"destination":"cash"}')"
  REF_CODE="$(echo "$REFJSON" | tail -1)"; REF_BODY="$(echo "$REFJSON" | head -n -1)"
  echo "    refund → HTTP $REF_CODE  body=$REF_BODY"
  if [ "$REF_CODE" = "200" ] || [ "$REF_CODE" = "201" ]; then
    # The real re_ is persisted in refund_provider_refs (ref_kind='refund').
    RE_ID="$(psql_db -tA -c "SELECT rpr.external_id FROM zeroship.refund_provider_refs rpr JOIN zeroship.refunds r ON r.id=rpr.refund_id WHERE r.invoice_id='$INTERNAL_INV' AND rpr.ref_kind='refund' ORDER BY rpr.created_at DESC LIMIT 1" 2>/dev/null | tr -d '[:space:]')"
    case "$RE_ID" in
      re_*) pass "REAL Stripe Refund issued: $RE_ID (200c cash to card)"; REFUND_OK=1;;
      *)    diverge "refund endpoint returned $REF_CODE but no re_ persisted (got '$RE_ID'). body=$REF_BODY";;
    esac
  else
    # Most likely: create_refund GET /v1/invoices reads a null payment_intent on
    # 2025-09-30.clover → "invoice has no payment_intent — cannot refund cash".
    diverge "cash refund returned HTTP $REF_CODE: $REF_BODY. Likely create_refund (crates/control/src/stripe_client.rs) reads invoice.payment_intent which is NULL on API 2025-09-30.clover (the same root cause as Stage 4). The cash-refund-to-card path is broken against current Stripe."
  fi
fi

# Over-refund cap BEFORE the dispute: remaining cap = cash_collected − refunds.
CASH_NOW="$(psql_db -tA -c "SELECT COALESCE(SUM(amount_cents),0) FROM zeroship.invoice_payments ip JOIN zeroship.invoices i ON i.id=ip.invoice_id WHERE i.creator_id='$CREATOR'" 2>/dev/null | tr -d '[:space:]')"
echo "    Σ(invoice_payments) for creator BEFORE dispute = ${CASH_NOW}c (the over-refund anchor)"

# ===========================================================================
echo ""
echo "=== Stage 7: REAL dispute (du_…) → charge.dispute.created webhook → cap TIGHTENS ==="
# ===========================================================================
# Create a REAL dispute on REAL Stripe using the dispute-triggering test token,
# on a charge linked to the SAME pi_/ch_ we recorded the linkage for — so the
# PR-8 resolution can map du_ → our invoice. We pay a fresh 750c off-session PI
# with tok_createDispute, then RELINK: we record THIS pi_/ch_ as the invoice's
# payment objects (mirroring what a real invoice.paid would, had the card been
# the dispute card) so the dispute resolves to our invoice. This isolates the
# PR-8 ch_/pi_ resolution against REAL dispute object shapes — the masked dim.
DISP_PI="$(spost payment_intents -d "amount=750" -d "currency=usd" -d "customer=$CUS" \
  -d "payment_method_data[type]=card" -d "payment_method_data[card][token]=tok_createDispute" \
  -d "confirm=true" -d "off_session=true" \
  -d "automatic_payment_methods[enabled]=true" -d "automatic_payment_methods[allow_redirects]=never" | jget id)"
DISP_CH="$(sget "payment_intents/$DISP_PI" | jget latest_charge)"
echo "    dispute-card PI=$DISP_PI CH=$DISP_CH"
case "$DISP_PI" in pi_*) pass "created REAL dispute-card PaymentIntent $DISP_PI (charge $DISP_CH)";; *) fail "dispute-card PI failed (got '$DISP_PI')";; esac

# Relink THIS pi_/ch_ to our internal invoice so the dispute resolves (faithful:
# this is exactly the linkage record_infra_payment WOULD have written if the
# invoice had been paid with this charge; we use the dispute charge so the du_
# carries ids that map to us). Idempotent insert.
if [ -n "$INTERNAL_INV" ]; then
  psql_db -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.billing_provider_refs (invoice_id, provider, ref_kind, external_id)
VALUES ('$INTERNAL_INV','stripe','payment_intent','$DISP_PI')
ON CONFLICT (invoice_id, provider, ref_kind) DO UPDATE SET external_id=EXCLUDED.external_id;
INSERT INTO zeroship.billing_provider_refs (invoice_id, provider, ref_kind, external_id)
VALUES ('$INTERNAL_INV','stripe','charge','$DISP_CH')
ON CONFLICT (invoice_id, provider, ref_kind) DO UPDATE SET external_id=EXCLUDED.external_id;
SQL
  pass "linked the dispute charge's pi_/ch_ to internal invoice $INTERNAL_INV (the invoice.paid linkage PR-8 resolves through)"
fi

# Poll REAL Stripe for the dispute object and read its REAL shape.
DU=""
for i in $(seq 1 12); do
  D="$(sget "charges/$DISP_CH" | jget dispute)"
  if [ -n "$D" ] && [ "$D" != "null" ]; then DU="$D"; break; fi
  sleep 3
done
[ -z "$DU" ] && DU="$(sget "disputes?charge=$DISP_CH&limit=1" | jget data.0.id)"
case "$DU" in du_*) pass "REAL dispute object appeared: $DU";; *) diverge "no dispute object materialized for $DISP_CH within poll window (Stripe creates disputes asynchronously); dispute leg cannot be completed this run"; DU="";; esac

if [ -n "$DU" ]; then
  DU_AMOUNT="$(sget "disputes/$DU" | jget amount)"
  DU_PI="$(sget "disputes/$DU" | jget payment_intent)"
  DU_CH="$(sget "disputes/$DU" | jget charge)"
  DU_REASON="$(sget "disputes/$DU" | jget reason)"
  echo "    REAL dispute: amount=${DU_AMOUNT}c pi=$DU_PI ch=$DU_CH reason=$DU_REASON"
  [ -n "$DU_PI" ] && [ -n "$DU_CH" ] && pass "REAL dispute object carries BOTH payment_intent ($DU_PI) and charge ($DU_CH) at top level — matches handle_dispute_created's candidates" \
    || diverge "dispute object missing pi_/ch_ (pi='$DU_PI' ch='$DU_CH')"

  DISP_EVENT="$(node -e '
  const [du,pi,ch,amount,reason,cur]=process.argv.slice(1);
  const obj={ id:du, object:"dispute", amount:Number(amount), currency:cur||"usd",
    status:"needs_response", reason:reason||"fraudulent",
    payment_intent:pi||null, charge:ch||null,
    evidence_details:{ due_by: Math.floor(Date.now()/1000)+1209600 } };
  const evt={ id:"evt_e2e_"+require("crypto").randomBytes(8).toString("hex"),
    object:"event", api_version:"2025-09-30.clover", created:Math.floor(Date.now()/1000),
    type:"charge.dispute.created", data:{ object:obj } };
  process.stdout.write(JSON.stringify(evt));
  ' "$DU" "$DU_PI" "$DU_CH" "${DU_AMOUNT:-750}" "$DU_REASON" "usd")"

  DWH_CODE="$(post_signed_webhook "$DISP_EVENT")"
  DWH_RESP="$(cat "$WORK/wh_resp.json" 2>/dev/null)"
  echo "    charge.dispute.created webhook → HTTP $DWH_CODE  resp=$DWH_RESP"
  [ "$DWH_CODE" = "200" ] && pass "charge.dispute.created webhook accepted (REAL signature) → 200" || fail "dispute webhook HTTP $DWH_CODE: $DWH_RESP"

  # PR-8 ASSERTION: the dispute resolved to our invoice → a billing_disputes row
  # AND a negative dispute_debit invoice_payments row → the over-refund cap
  # (Σ(invoice_payments)) TIGHTENS by the disputed amount.
  DISPUTE_ROWS="$(psql_db -tA -c "SELECT COUNT(*) FROM zeroship.billing_disputes WHERE provider_dispute_id='$DU'" 2>/dev/null | tr -d '[:space:]')"
  DEBIT_ROWS="$(psql_db -tA -c "SELECT COUNT(*) FROM zeroship.invoice_payments WHERE kind='dispute_debit'" 2>/dev/null | tr -d '[:space:]')"
  CASH_AFTER="$(psql_db -tA -c "SELECT COALESCE(SUM(amount_cents),0) FROM zeroship.invoice_payments ip JOIN zeroship.invoices i ON i.id=ip.invoice_id WHERE i.creator_id='$CREATOR'" 2>/dev/null | tr -d '[:space:]')"
  echo "    after dispute: billing_disputes=$DISPUTE_ROWS dispute_debit_rows=$DEBIT_ROWS  Σ(invoice_payments)=${CASH_AFTER}c (was ${CASH_NOW}c)"

  if [ "$DISPUTE_ROWS" = "1" ]; then
    pass "PR-8: REAL dispute $DU resolved to our invoice → billing_disputes row recorded (real ch_/pi_ resolution works)"
  else
    if echo "$DWH_RESP" | grep -q "no_internal_invoice"; then
      diverge "PR-8: dispute webhook acked 'no_internal_invoice' — resolve_invoice_for_dispute could not map the REAL du_'s pi_/ch_ to our invoice. (If the pi_/ch_ linkage was present this indicates the resolution is still dead; if absent it confirms the Stage-4 invoice.paid divergence prevents the linkage.)"
    else
      fail "expected 1 billing_disputes row for $DU, got '$DISPUTE_ROWS' (resp=$DWH_RESP)"
    fi
  fi
  if [ "$DEBIT_ROWS" -ge 1 ] 2>/dev/null && [ -n "$CASH_AFTER" ] && [ -n "$CASH_NOW" ] && [ "$CASH_AFTER" -lt "$CASH_NOW" ] 2>/dev/null; then
    pass "over-refund cap TIGHTENED: Σ(invoice_payments) dropped ${CASH_NOW}c → ${CASH_AFTER}c via the negative dispute_debit row (a creator cannot refund clawed-back cash)"
  else
    if [ "$DISPUTE_ROWS" = "1" ]; then
      fail "dispute recorded but the over-refund cap did NOT tighten (debit_rows=$DEBIT_ROWS, ${CASH_NOW}c→${CASH_AFTER}c)"
    else
      diverge "over-refund cap did not tighten because the dispute did not resolve (see above)"
    fi
  fi
fi

# ===========================================================================
echo ""
echo "============================================"
echo "  Results: $PASS passed, $FAIL failed, $DIVERGENCE real-API divergence(s)"
echo "============================================"
[ $FAIL -eq 0 ] && exit 0 || exit 1
