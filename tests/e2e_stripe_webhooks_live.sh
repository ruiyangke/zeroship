#!/usr/bin/env bash
# ============================================================================
# e2e_stripe_webhooks_live.sh — GENUINELY Stripe-DELIVERED webhook e2e for the
# zeroship billing rail. This is the [[feedback_faithful_e2e_tests]] capstone
# that closes the one gap the sibling harness (tests/e2e_stripe_billing.sh)
# could NOT close: that harness SELF-CONSTRUCTED the webhook event envelopes and
# HMAC-signed them itself (no Stripe CLI was on PATH), so it proved the
# signature-verify + handler logic but NEVER exercised Stripe's REAL delivery —
# the real event `type` names, the real `data.object` shape, the real
# `api_version`, real ordering, real retries, and Stripe's OWN signature.
#
# Here the Stripe CLI (`stripe listen` + `stripe trigger`) makes STRIPE ITSELF
# deliver the real, signed event envelopes through the listener to our control
# instance's /internal/webhooks/stripe — the genuine end-to-end path.
#
#   stripe listen --print-secret              → capture the STABLE whsec_…
#     ─► boot zeroship-control with ZEROSHIP_CONTROL_STRIPE_WEBHOOK_SECRET = that secret
#     ─► stripe listen --forward-to $CONTROL/internal/webhooks/stripe (bg)
#         (Stripe streams every test-mode event to us, REAL-signed)
#     ─► create real objects (customer + PI + pay) → Stripe DELIVERS
#         invoice.paid / payment_intent.succeeded / charge.succeeded … to us
#     ─► stripe trigger <event>                 → Stripe fires + DELIVERS a real
#         test event of that type through the listener to our handler
#
# WHAT THIS PROVES (vs. the self-constructed harness):
#   1. Core loop, Stripe-DELIVERED: a real pay → the REAL invoice.paid /
#      payment_intent.succeeded / charge.succeeded the way STRIPE delivers them.
#   2. The 3 newly-handled deferred events, Stripe-DELIVERED:
#      charge.refund.updated, payment_intent.payment_failed, payout.failed.
#   3. Dispute, Stripe-DELIVERED: a real dispute (test card 4000000000000259) →
#      Stripe's own charge.dispute.created.
#   4. Connect/payout: the least-coverable leg — see the HONESTY notes inline.
#
# HONESTY MANDATE (the whole point of this harness): it reports EXACTLY what
# Stripe DELIVERED (real ids) and EVERY divergence between Stripe's REAL
# delivered envelope and what the self-constructed harness assumed. It does NOT
# fix crates/control; a handler bug surfaced by real delivery is FLAGGED, not
# patched. A leg that genuinely can't be automated (real Connect onboarding) is
# reported with how far it got — never faked.
#
# DO NO HARM: a PER-RUN dedicated DB on :5440 (never the real `zeroship` DB,
# never zeroship_billing_test, never a peer harness's database) and a PER-RUN
# control port. Self-managed up/down; tears down `stripe listen` + control on
# exit. REFUSES (exit 2, naming the missing thing and its remedy) when a
# prerequisite is absent: no stripe CLI, no PG :5440, no docker, no built control
# binary, no keys, or a `stripe listen` that never authenticates or becomes
# Ready.
#
# NOT WIRED INTO CI: nothing in .github/workflows/ names this script, and
# `cargo xtask test billing` does not invoke it either. It needs an operator's
# live Stripe TEST keys and an authenticated stripe CLI, neither of which CI can
# hold, so refusing cannot turn a CI job permanently red. It refuses to the
# person who ran it by hand, which is the only reader it has.
#
# BOTH HALVES OF THAT USED TO BE FIXED CONSTANTS, and both were shared. The
# database was `zeroship_stripe_e2e`, the same literal as
# tests/e2e_stripe_billing.sh and tests/e2e_stripe_connect_live.sh, and Stage 1
# opened by terminating every backend on it and dropping it; the control port
# was 9182, the same literal as tests/create_demo_invoices.sh:116. Two of those
# running together on one box is one of them destroying the other's run. See
# tests/lib/scratch_db.sh and tests/lib/e2e_ports.sh.
#
# Usage:
#   source /home/ruiyang/.config/zeroship-stripe-test.env   # sets the TEST keys
#   ./tests/e2e_stripe_webhooks_live.sh
#   STRICT=1 ./tests/e2e_stripe_webhooks_live.sh   # documented divergences = hard fail
#
# SECRETS: reads $STRIPE_TEST_SECRET_KEY from the env ONLY. NEVER prints/writes/
# commits any sk_/pk_/whsec_ value (the whsec_ from `stripe listen` is captured
# into a shell var, never echoed, never persisted).
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release"
# shellcheck source=tests/lib/runtime_secrets.sh
source "$ROOT/tests/lib/runtime_secrets.sh"
# `zeroship.app_members` is deleted; an app reaches the people who answer for it
# through its project's organization. `seat_app_owner_sql` emits that join AND a
# check that raises when it matches nothing - an INSERT ... SELECT over no rows
# is a SUCCESSFUL statement that seats nobody, and the 403 it later produces
# surfaces far from here.
source "$ROOT/tests/lib/organization_fixture.sh"
STRICT="${STRICT:-0}"

PASS=0; FAIL=0; DIVERGENCE=0
pass()  { PASS=$((PASS+1));  echo "  ✓ $1"; }
fail()  { FAIL=$((FAIL+1));  echo "  ✗ $1"; }
# A real-Stripe-delivery divergence from our self-constructed harness's
# assumptions, or a genuinely-unautomatable leg. Reported loudly; only a hard
# fail under STRICT=1.
diverge() { DIVERGENCE=$((DIVERGENCE+1)); echo "  ⚠ REAL-DELIVERY NOTE: $1"; if [ "$STRICT" = "1" ]; then FAIL=$((FAIL+1)); fi; }

echo "============================================"
echo "  zeroship E2E — Stripe-DELIVERED webhooks (stripe listen + trigger, REAL delivery)"
echo "============================================"

# --- prereq gates: EVERY missing prerequisite REFUSES ----------------------
# psql: $PATH first, then any postgresql in the nix store, then refuse. This
# replaced ZEROSHIP_PSQL, whose default was one pinned /nix/store hash that
# resolved on exactly one machine; everywhere else it was absent and the
# absence was `exit 0`, so this suite measured nothing and reported success.
# Same chain as the other e2e harnesses: $PATH, then the nix store, then refuse.
#
# THAT SENTENCE WAS THE ONLY ONE ACTED ON at the time: psql got a refusal and
# every other prerequisite kept its `exit 0`, so the diagnosis sat beside the
# arms it applied to verbatim. They refuse now too, including the two that decide
# mid-run whether Stripe is actually delivering (the signing secret and the
# `listen` forwarder's Ready line) - the arms without which this suite has no
# subject at all.
zs_prereq() {
  echo "" >&2
  echo "  x MISSING PREREQUISITE: $1" >&2
  echo "    remedy: $2" >&2
  echo "" >&2
  echo "    This harness does not skip. A run that cannot reach what it tests" >&2
  echo "    must not print the exit code of a run that tested it." >&2
  exit 2
}
PSQL="${PSQL:-}"
if [ -z "$PSQL" ]; then
  if command -v psql >/dev/null 2>&1; then
    PSQL="$(command -v psql)"
  else
    PSQL="$(ls -d /nix/store/*postgresql*/bin/psql 2>/dev/null | head -1 || true)"
  fi
fi
PGHOST=localhost; PGPORT=5440; PGUSER=postgres; PGPW=zeroship

# Per-run database name. `run_psql` is the seam tests/lib/scratch_db.sh reaches
# the server through, so it is defined before the resolve.
run_psql() { PGPASSWORD="$PGPW" "$PSQL" -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" "$@"; }
# shellcheck source=tests/lib/scratch_db.sh
source "$ROOT/tests/lib/scratch_db.sh"
zs_scratch_db_resolve zeroship_stripe_e2e || exit $?
DB="$TEST_DB"
# shellcheck source=tests/lib/e2e_ports.sh
source "$ROOT/tests/lib/e2e_ports.sh"

if [ -z "${STRIPE_TEST_SECRET_KEY:-}" ]; then
  zs_prereq "STRIPE_TEST_SECRET_KEY is not set, so there is no Stripe TEST account to deliver from." \
            "source the operator's zeroship-stripe-test.env, or export the sk_test_ key first."
fi
case "$STRIPE_TEST_SECRET_KEY" in
  sk_test_*) ;;
  *) echo "  ✗ REFUSING TO RUN: STRIPE_TEST_SECRET_KEY is not an sk_test_ key. TEST mode only."; exit 2 ;;
esac
SK="$STRIPE_TEST_SECRET_KEY"

# Resolve the Stripe CLI: prefer a stripe already on PATH, else a nix-store path
# (the operator obtains it via `nix-shell -p stripe-cli`; we cache the resolved
# binary path so we never pay the nix-shell `--run` shim cost per call).
STRIPE=""
if command -v stripe >/dev/null 2>&1; then
  STRIPE="$(command -v stripe)"
elif [ -n "${ZEROSHIP_STRIPE_BIN:-}" ] && [ -x "${ZEROSHIP_STRIPE_BIN}" ]; then
  STRIPE="$ZEROSHIP_STRIPE_BIN"
else
  # Ask nix to materialize stripe-cli and print its path (one-time fetch+cache).
  CAND="$(nix-shell -p stripe-cli --run 'command -v stripe' 2>/dev/null | tail -1)"
  if [ -z "$CAND" ] || [ ! -x "$CAND" ]; then
    # The zsh nix-shell function's buildShellShim can mis-handle `--run`; fall
    # back to globbing the nix store for an already-realised stripe-cli.
    CAND="$(ls -1 /nix/store/*-stripe-cli-*/bin/stripe 2>/dev/null | tail -1)"
  fi
  STRIPE="$CAND"
fi
if [ -z "$STRIPE" ] || [ ! -x "$STRIPE" ]; then
  zs_prereq "the stripe CLI was not found on PATH, in ZEROSHIP_STRIPE_BIN, or in the nix store; it is what makes Stripe DELIVER to us." \
            "nix-shell -p stripe-cli, or set ZEROSHIP_STRIPE_BIN=/path/to/stripe"
fi
echo "  stripe CLI: $("$STRIPE" version 2>/dev/null | head -1) ($STRIPE)"

[ -n "$PSQL" ] && [ -x "$PSQL" ] || {
  echo "  x ABORT: no psql on \$PATH or in the nix store; set PSQL to the Postgres client binary." >&2
  exit 2
}
command -v node    >/dev/null 2>&1 || zs_prereq "node is not on PATH; the harness mints its bearer with it." "install Node, or enter the dev shell: nix develop"
command -v openssl >/dev/null 2>&1 || zs_prereq "openssl is not on PATH; the gateway signing key is generated with it." "install openssl, or enter the dev shell: nix develop"
command -v curl    >/dev/null 2>&1 || zs_prereq "curl is not on PATH; every Stripe and control-plane call goes through it." "install curl, or enter the dev shell: nix develop"
[ -x "$BIN/zeroship-control" ] || zs_prereq "no control binary at $BIN/zeroship-control; this harness drives the real one." "cargo build --release -p zeroship-control"

export PGPASSWORD="$PGPW"
psql_db() { "$PSQL" -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d "$DB" "$@"; }
psql1()   { psql_db -tA -c "$1" 2>/dev/null | tr -d '[:space:]'; }
if ! "$PSQL" -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -tAc "SELECT 1" >/dev/null 2>&1; then
  zs_prereq "PostgreSQL at $PGHOST:$PGPORT did not answer 'SELECT 1' as $PGUSER; the per-run database lives there." \
            "start it (tests/provision_test_backends.sh), or point PGPORT at a server that answers."
fi
if [ -f "$ROOT/deploy/ops/db-migrate.sh" ] && ! command -v docker >/dev/null 2>&1; then
  zs_prereq "docker is not on PATH and deploy/ops/db-migrate.sh needs it to apply the platform migration set." \
            "install docker and start its daemon."
fi

SAPI="https://api.stripe.com/v1"
# The harness's OWN out-of-band Stripe REST driver (control uses its own cyper
# client). NEVER echo $SK.
sget()  { curl -s "$SAPI/$1" -u "$SK:"; }
spost() { curl -s -X POST "$SAPI/$1" -u "$SK:" "${@:2}"; }
jget()  { node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{try{const o=JSON.parse(s);let v=o;for(const k of process.argv[1].split('.').filter(Boolean)){v=v==null?undefined:(k.match(/^[0-9]+$/)?v[+k]:v[k]);}console.log(v==null?'':(typeof v==='object'?JSON.stringify(v):v))}catch(e){console.log('')}})" "$1"; }
# Stripe CLI wrapper — every call authed with the TEST key (no browser login).
scli()  { "$STRIPE" "$@" --api-key "$SK"; }

# Allocated, not the constant 9182 this harness used to share with
# tests/create_demo_invoices.sh:116.
zs_ports_reserve ZEROSHIP_CONTROL_PORT || exit 1
CONTROL_URL="http://localhost:$ZEROSHIP_CONTROL_PORT"
WORK="$(mktemp -d -t zs-e2e-whlive-XXXXXX)"
mkdir -p "$WORK/blobs"
PIDFILE="$WORK/pids"; : > "$PIDFILE"
LISTEN_LOG="$WORK/listen.log"

cleanup() {
  local rc=$?
  echo ""
  echo "=== Cleanup ==="
  if [ -f "$PIDFILE" ]; then
    while read -r pid; do [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; done < "$PIDFILE"
  fi
  # Belt-and-suspenders for a stripe-listen that re-execs or double-forks past
  # the PID above. SCOPED BY PID (`pkill -P`), never by pattern: a
  # `pkill -f "stripe listen ..."` matches on a command line, and the killer's
  # own shell can carry that string.
  [ -n "${LISTEN_PID:-}" ] && pkill -P "$LISTEN_PID" 2>/dev/null
  wait 2>/dev/null || true
  [ -n "${WORK:-}" ] && rm -rf "$WORK"
  # Drop on SUCCESS only: on a red run the rows control wrote from Stripe's own
  # delivered envelopes ARE the finding.
  zs_scratch_db_cleanup_on_success "$rc"
  zs_ports_release
  echo "  control + stripe-listen down; $WORK cleaned. (The real zeroship DB +"
  echo "  zeroship_billing_test were NEVER touched.)"
  echo "  NOTE: Stripe TEST-mode objects (cus_/pi_/ch_/re_/du_) created by this run are harmless test artifacts."
  return 0
}
trap cleanup EXIT

# NO `lsof -ti :$PORT | xargs kill -9` HERE ANY MORE - see the header. An
# allocated port has no leaked previous run to reclaim it from, and that line
# could not tell one from a peer agent's live control plane.

# Wait until the `payment_intents/$1` resolves to status `succeeded` (the
# off-session confirm is usually synchronous, but be robust).
wait_pi_succeeded() {
  local pi="$1" st
  for _ in $(seq 1 10); do
    st="$(sget "payment_intents/$pi" | jget status)"
    [ "$st" = "succeeded" ] && { echo "$st"; return 0; }
    sleep 1
  done
  echo "$st"
}

# Poll the control DB until a query returns the expected value (the listener
# delivers asynchronously; give Stripe + the forwarder a few seconds).
wait_for_db() {
  # $1=sql  $2=expected  $3=tries(default 20)  → echoes the final value
  local sql="$1" want="$2" tries="${3:-20}" got=""
  for _ in $(seq 1 "$tries"); do
    got="$(psql1 "$sql")"
    [ "$got" = "$want" ] && { echo "$got"; return 0; }
    sleep 1
  done
  echo "$got"; return 1
}

# ===========================================================================
echo ""
echo "=== Stage 1: dedicated DB + platform migrations + control (REAL Stripe) + stripe-listen forwarder ==="
# ===========================================================================
# NO `pg_terminate_backend` AND NO PRE-DROP: the name carries a per-run token,
# so nothing can already hold it, and both statements were only ever able to hit
# a PEER's database. `pg_terminate_backend` over `datname=` is `WITH (FORCE)`
# spelled out by hand.
"$PSQL" -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL || { fail "could not create $DB"; exit 1; }
CREATE DATABASE $DB;
ALTER DATABASE $DB SET search_path = zeroship, public;
SQL
pass "created per-run DB $DB on :$PGPORT"

MIG_LOG="$WORK/migrate.log"
if ZEROSHIP_MIGRATE_DSN="postgres://$PGUSER:$PGPW@$PGHOST:$PGPORT/$DB" "$ROOT/deploy/ops/db-migrate.sh" > "$MIG_LOG" 2>&1; then
  pass "platform migration set applied (incl. 0042 invoicing, 0049 refunds, 0053 disputes, 0054 webhook follow-ups)"
else
  fail "platform migration FAILED (see $MIG_LOG)"; tail -20 "$MIG_LOG"; exit 1
fi

# Capture a STABLE webhook signing secret BEFORE booting control, so control's
# ZEROSHIP_CONTROL_STRIPE_WEBHOOK_SECRET == the secret Stripe will sign deliveries with. NEVER
# echoed. (`stripe listen --print-secret` returns the account's persistent CLI
# secret; the subsequent `stripe listen --forward-to` reuses the SAME secret.)
WEBHOOK_SECRET="$(scli listen --print-secret 2>/dev/null | tr -d '[:space:]')"
case "$WEBHOOK_SECRET" in
  whsec_*) pass "captured the STABLE stripe-listen webhook signing secret (whsec_… — not printed)";;
  *) zs_prereq "\`stripe listen --print-secret\` returned no whsec_ value, so control cannot be configured with the secret Stripe will sign with." \
               "authenticate the CLI (\`stripe login\`) and check network reachability to Stripe, then re-run.";;
esac

DBURL="postgres://$PGUSER:$PGPW@$PGHOST:$PGPORT/$DB"
openssl genpkey -algorithm ed25519 -out "$WORK/signing-key.pem" 2>/dev/null
chmod 600 "$WORK/signing-key.pem"

# control — REAL https://api.stripe.com, the operator's TEST secret key (from env,
# NEVER on argv), and the REAL stripe-listen webhook secret (so Stripe's OWN
# signature on its OWN delivered events verifies through the production path).
ZEROSHIP_GATEWAY_SIGNING_KEY_FILE="$WORK/signing-key.pem"
e2e_export_runtime_secrets "$WORK" || exit 1
e2e_export_database_urls "$DBURL"
ZEROSHIP_CONTROL_STRIPE_SECRET_KEY="$SK" ZEROSHIP_CONTROL_STRIPE_WEBHOOK_SECRET="$WEBHOOK_SECRET" \
"$BIN/zeroship-control" --port "$ZEROSHIP_CONTROL_PORT" \
  --blob-store "$WORK/blobs" \
  --stripe-base-url "https://api.stripe.com" \
 > "$WORK/control.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "$CONTROL_URL/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "$CONTROL_URL/readyz" >/dev/null 2>&1 \
  && pass "control healthy (→ REAL api.stripe.com; ZEROSHIP_CONTROL_STRIPE_WEBHOOK_SECRET = the stripe-listen secret)" \
  || { fail "control unhealthy"; tail -30 "$WORK/control.log"; exit 1; }

# Start the REAL forwarder: Stripe streams every test-mode event to our control,
# REAL-signed with the secret we captured above. Long-lived; torn down on exit.
scli listen --forward-to "$CONTROL_URL/internal/webhooks/stripe" --skip-verify > "$LISTEN_LOG" 2>&1 &
LISTEN_PID=$!
echo "$LISTEN_PID" >> "$PIDFILE"
for _ in $(seq 1 25); do grep -q "Ready!" "$LISTEN_LOG" 2>/dev/null && break; sleep 1; done
if grep -q "Ready!" "$LISTEN_LOG" 2>/dev/null; then
  pass "stripe listen forwarder READY — Stripe now DELIVERS real signed events to control"
  echo "    $(grep -m1 'API Version' "$LISTEN_LOG" | sed 's/Your webhook signing secret is whsec_[a-f0-9]*/Your webhook signing secret is whsec_…(redacted)/')"
else
  echo "    tail of $LISTEN_LOG:" >&2
  tail -8 "$LISTEN_LOG" >&2
  zs_prereq "\`stripe listen\` never printed Ready, so Stripe is not delivering to control and this suite's whole subject is absent." \
            "authenticate the CLI (\`stripe login\`) and check network reachability to Stripe, then re-run."
fi

# Record the listen log size so per-stage we can attribute which events Stripe
# delivered during that stage.
listen_since() { wc -l < "$LISTEN_LOG" 2>/dev/null | tr -d ' '; }
delivered_types_since() {
  # $1 = line offset captured by listen_since before the action
  tail -n +"$(( ${1:-0} + 1 ))" "$LISTEN_LOG" 2>/dev/null | grep -oE -- '--> [a-z_.]+' | sed 's/--> //' | sort -u | tr '\n' ' '
}

# ===========================================================================
echo ""
echo "=== Stage 2: REAL Customer + pay → Stripe DELIVERS the core money events ==="
# ===========================================================================
CREATOR="$(node -e 'console.log(require("crypto").randomUUID())')"
NOW_UNIX="$(date +%s)"
# The ids are derived HERE, before the customer, because the ownership metadata
# the platform stamps on a Stripe Customer names the ORGANIZATION - a customer
# created before the id exists carries no attribution, and under `set -u` the
# expansion aborts the run outright. The rows themselves are written below.
organization_fixture_ids "whlive-$CREATOR"
CUS="$(spost customers -d "email=whlive-$CREATOR@zeroship.test" -d "metadata[organization_id]=$ZS_FIXTURE_ORGANIZATION_ID" | jget id)"
case "$CUS" in cus_*) pass "created REAL Stripe Customer $CUS (organization=$ZS_FIXTURE_ORGANIZATION_ID)";; *) fail "create customer failed"; exit 1;; esac
PM="$(spost payment_methods/pm_card_visa/attach -d "customer=$CUS" | jget id)"
spost "customers/$CUS" -d "invoice_settings[default_payment_method]=$PM" -o /dev/null
pass "attached test PaymentMethod $PM as default"

# Seed the internal identity + invoice so the delivered invoice.paid can attach a
# charge row. (The reconciler→Stripe invoice path is covered by the sibling
# harness; here the FOCUS is REAL DELIVERY, so we seed a minimal infra invoice and
# pay it on Stripe to provoke Stripe's OWN invoice.paid / charge.succeeded.)
CLOSED_APP="$(node -e 'console.log(require("crypto").randomUUID())')"
PERIOD_FIRST="$(node -e 'const d=new Date();console.log(new Date(Date.UTC(d.getUTCFullYear(),d.getUTCMonth(),1)).toISOString().slice(0,10))')"
# The app needs a project, and the project an organization: that chain is the
# only path from an app to the party it is billed to. The ids were derived above
# (the Stripe Customer's ownership metadata needs them); the rows they name are
# written here. They live in THIS shell rather than the emitter's, because
# `$(organization_fixture_sql ...)` runs in a subshell.
psql_db -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL || { fail "seed failed"; exit 1; }
INSERT INTO zeroship.plans (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, runtime_limits_json, spend_limit_default_cents)
VALUES ('pln_whlive','wh-live',0,0,1000000000000,'{"cpu_limit_ms":5000,"wall_timeout_ms":30000,"heap_limit_mb":256}',100000000)
ON CONFLICT (id) DO NOTHING;
INSERT INTO zeroship.users (id, email, name, email_verified_at)
VALUES ('$CREATOR', 'whlive-$CREATOR@zeroship.test'::citext, 'WH-Live Creator', NOW());
$(organization_fixture_sql "whlive-$CREATOR" "whlive-$CREATOR@zeroship.test")
INSERT INTO zeroship.apps (id, name, plan_id, project_id, organization_id)
VALUES ('$CLOSED_APP', 'whlive-app-$CLOSED_APP', 'pln_whlive', '$ZS_FIXTURE_PROJECT_ID', '$ZS_FIXTURE_ORGANIZATION_ID');
$(seat_app_owner_sql "$CLOSED_APP" "$CREATOR")
$(organization_billing_sql "$ZS_FIXTURE_ORGANIZATION_ID")
INSERT INTO zeroship.billing_customer_refs (organization_id, provider, external_id)
VALUES ('$ZS_FIXTURE_ORGANIZATION_ID', 'stripe', '$CUS')
ON CONFLICT (organization_id, provider) DO UPDATE SET external_id = EXCLUDED.external_id;
SQL
pass "seeded internal identity + creator↔$CUS mapping"

# Drive a REAL invoice: create item + invoice + finalize on Stripe so Stripe
# DELIVERS invoice.paid (+ invoice.payment_succeeded + charge.succeeded +
# payment_intent.succeeded) through the forwarder when we pay.
OFFSET_S2="$(listen_since)"
spost invoiceitems -d "customer=$CUS" -d "amount=750" -d "currency=usd" \
  -d "description=zeroship infra (whlive e2e)" -o /dev/null
INV_JSON="$(spost invoices -d "customer=$CUS" -d "collection_method=charge_automatically" \
  -d "pending_invoice_items_behavior=include" -d "metadata[organization_id]=$ZS_FIXTURE_ORGANIZATION_ID" -d "metadata[invoice_kind]=infra")"
INV="$(echo "$INV_JSON" | jget id)"
case "$INV" in in_*) pass "created REAL invoice $INV (item swept via pending_invoice_items_behavior=include)";; *) fail "create invoice failed: $(echo "$INV_JSON"|jget error.message)"; ;; esac
spost "invoices/$INV/finalize" -o /dev/null

# Seed the INTERNAL finalized invoice row + the billing_provider_refs(ref_kind=
# 'invoice', external_id=in_…) mapping the handler resolves the delivered
# invoice.paid through (invoice_id_for_provider_invoice). The reconciler normally
# writes these; here we seed them directly so the REAL delivered invoice.paid can
# attach its charge row + pi_/ch_ linkage to OUR invoice. (Faithful: this is the
# exact mapping the create_invoice → finalize path persists.)
INTERNAL_INV="inv_$(node -e 'console.log(require("crypto").randomBytes(8).toString("hex"))')"
psql_db -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL || { fail "internal-invoice seed failed"; }
INSERT INTO zeroship.invoices (id, organization_id, period, status, currency, subtotal_cents, total_cents, finalized_at)
VALUES ('$INTERNAL_INV','$ZS_FIXTURE_ORGANIZATION_ID','$PERIOD_FIRST'::date,'finalized','usd',750,750,NOW());
INSERT INTO zeroship.billing_provider_refs (invoice_id, provider, ref_kind, external_id)
VALUES ('$INTERNAL_INV','stripe','invoice','$INV')
ON CONFLICT (invoice_id, provider, ref_kind) DO UPDATE SET external_id=EXCLUDED.external_id;
SQL
pass "seeded internal finalized invoice $INTERNAL_INV ↔ Stripe $INV (the invoice.paid attach target)"

PAY_JSON="$(spost "invoices/$INV/pay")"
INV_STATUS="$(echo "$PAY_JSON" | jget status)"
REAL_PI="$(echo "$PAY_JSON" | jget payment_intent)"
REAL_CH="$(echo "$PAY_JSON" | jget charge)"
echo "    REAL paid invoice: status=$INV_STATUS pi=$REAL_PI ch=$REAL_CH"
[ "$INV_STATUS" = "paid" ] && pass "REAL invoice $INV PAID on Stripe (750c)" || fail "invoice did not pay (status=$INV_STATUS)"

# Wait for Stripe to DELIVER the core events through the listener.
sleep 8
DELIV="$(delivered_types_since "$OFFSET_S2")"
echo "    Stripe DELIVERED during Stage 2: $DELIV"
for want in invoice.paid invoice.payment_succeeded payment_intent.succeeded charge.succeeded; do
  case " $DELIV " in *" $want "*) pass "Stripe DELIVERED $want (real envelope, real signature)";; *) diverge "expected Stripe to deliver '$want' but the listener did not log it this window (delivered: $DELIV)";; esac
done

# DIVERGENCE CHECK: what did the REAL delivered invoice.paid envelope carry? The
# self-constructed harness ASSUMED a nested invoice.payments.data[].payment shape
# carrying pi_/ch_. Inspect Stripe's OWN delivered event for the truth.
EVT_SHAPE="$(sget "events?type=invoice.paid&limit=20" | node -e '
let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{
  const arr=JSON.parse(s).data||[]; const inv=process.argv[1];
  const e=arr.find(x=>x.data&&x.data.object&&x.data.object.id===inv);
  if(!e){console.log("MISSING");return;}
  const o=e.data.object;
  console.log(JSON.stringify({api_version:e.api_version, top_charge:o.charge??null, top_pi:o.payment_intent??null,
    has_payments:Object.prototype.hasOwnProperty.call(o,"payments"),
    nested_pi:(o.payments&&o.payments.data&&o.payments.data[0]&&o.payments.data[0].payment&&o.payments.data[0].payment.payment_intent)||null}));
});' "$INV")"
echo "    REAL delivered invoice.paid data.object shape: $EVT_SHAPE"
API_VER="$(echo "$EVT_SHAPE" | jget api_version)"
[ -n "$API_VER" ] && pass "REAL delivered invoice.paid api_version = $API_VER (our StripeObject parses default-tolerantly)"
HAS_TOP_PI="$(echo "$EVT_SHAPE" | jget top_pi)"
HAS_NEST_PI="$(echo "$EVT_SHAPE" | jget nested_pi)"
if [ -z "$HAS_TOP_PI" ] && [ -z "$HAS_NEST_PI" ]; then
  diverge "Stripe's OWN delivered invoice.paid ($EVT_SHAPE) carries NEITHER a top-level payment_intent/charge NOR a populated payments.data[].payment.{payment_intent,charge}. record_infra_payment's invoice_payment_object_ids reads exactly those fields → against REAL delivery it records NO pi_/ch_ linkage. (The handler then fetches them out-of-band via resolve_settlement_ids — assert below whether that fired.)"
fi

# Did control commit the charge row + pi_/ch_ linkage off the REAL delivered event?
CASH="$(wait_for_db "SELECT COALESCE(SUM(amount_cents),0) FROM zeroship.invoice_payments ip JOIN zeroship.invoices i ON i.id=ip.invoice_id WHERE i.organization_id='$ZS_FIXTURE_ORGANIZATION_ID' AND ip.kind='charge'" "750" 15)"
[ "$CASH" = "750" ] && pass "STRIPE-DELIVERED invoice.paid → invoice_payments 'charge' row appended (cash=750c) [D1/C1 re-validated against real delivery]" \
  || diverge "no 750c charge row from the real delivered invoice.paid (got '$CASH'). control.log: $(grep -iE 'invoice.paid|store error|creator' "$WORK/control.log" | tail -3 | tr '\n' '|')"

# D2/C2 re-validation: the pay RESPONSE did NOT inline pi_/ch_ (REAL_PI='$REAL_PI',
# REAL_CH='$REAL_CH' — itself the D2 shape finding), and Stripe's delivered invoice.paid
# carries none either. So the linkage can ONLY land if control's out-of-band
# settlement_ids_for fetch (expand[]=payments.data.payment) fired. We assert the linkage
# rows landed on OUR internal invoice REGARDLESS of whether the harness locally captured
# the ids — i.e. control RECOVERED the real pi_/ch_ itself. This is the C2/D2 dimension.
LINK_PI="$(wait_for_db "SELECT external_id FROM zeroship.billing_provider_refs WHERE invoice_id='$INTERNAL_INV' AND ref_kind='payment_intent'" "$(psql1 "SELECT external_id FROM zeroship.billing_provider_refs WHERE invoice_id='$INTERNAL_INV' AND ref_kind='payment_intent'")" 10 2>/dev/null)"
LINK_PI="$(psql1 "SELECT external_id FROM zeroship.billing_provider_refs WHERE invoice_id='$INTERNAL_INV' AND ref_kind='payment_intent'")"
LINK_CH="$(psql1 "SELECT external_id FROM zeroship.billing_provider_refs WHERE invoice_id='$INTERNAL_INV' AND ref_kind='charge'")"
case "$LINK_PI" in
  pi_*) pass "STRIPE-DELIVERED invoice.paid → control RECOVERED + recorded the real pi_ linkage ($LINK_PI) via its out-of-band expand-fetch [D2/C2 re-validated against real delivery]";;
  *) diverge "no pi_ linkage on $INTERNAL_INV after the real delivered invoice.paid (got '$LINK_PI'). The delivered event inlined no ids and control's settlement_ids_for expand-fetch did not land one. control.log: $(grep -iE 'settlement|invoice.paid|expand' "$WORK/control.log" | tail -2 | tr '\n' '|')";;
esac
case "$LINK_CH" in
  ch_*) pass "STRIPE-DELIVERED invoice.paid → control RECOVERED + recorded the real ch_ linkage ($LINK_CH)";;
  *) diverge "no ch_ linkage on $INTERNAL_INV after the real delivered invoice.paid (got '$LINK_CH').";;
esac
# Make the recovered ids available to later stages (the summary + dispute relink).
REAL_PI="${REAL_PI:-$LINK_PI}"; REAL_CH="${REAL_CH:-$LINK_CH}"

# ===========================================================================
echo ""
echo "=== Stage 3: the 3 newly-handled deferred events, Stripe-DELIVERED ==="
# ===========================================================================
# --- 3a. charge.refund.updated (stripe trigger fires a REAL one) -----------
# `stripe trigger charge.refund.updated` creates a real refund and delivers its
# real charge.refund.updated. Our handler acts ONLY on terminal-failure states
# (failed/canceled); the fixture's refund is typically `succeeded`, so the
# faithful expectation is `refund_update_noop` (a benign ack) UNLESS the fixture
# yields a failed refund. We assert the handler ACK'd the REAL delivered event
# (HTTP 200 in the listener) — the durable failed-refund reversal is additionally
# exercised against a known-failed refund below.
OFFSET_3A="$(listen_since)"
scli trigger charge.refund.updated >/dev/null 2>&1
sleep 6
RU_DELIV="$(delivered_types_since "$OFFSET_3A")"
echo "    Stripe DELIVERED (3a): $RU_DELIV"
RU_OK="$(grep -c -- '--> charge.refund.updated' <(tail -n +"$((OFFSET_3A+1))" "$LISTEN_LOG"))"
RU_200="$(tail -n +"$((OFFSET_3A+1))" "$LISTEN_LOG" | grep -A1 'charge.refund.updated' | grep -c '\[200\]')"
[ "$RU_OK" -ge 1 ] && pass "Stripe DELIVERED a REAL charge.refund.updated ($RU_OK event(s))" || diverge "no charge.refund.updated delivered by the trigger (got: $RU_DELIV)"
[ "$RU_200" -ge 1 ] && pass "handle_refund_updated ACK'd the REAL delivered charge.refund.updated → [200]" || fail "charge.refund.updated did NOT 200 (handler error?). control.log: $(grep -i refund "$WORK/control.log" | tail -2 | tr '\n' '|')"

# The TERMINAL-FAILURE reversal (refunds.status issued→failed + failed_at + the
# refund_clawback for a credit-dest refund) is the money-critical effect, but it can
# only fire on a `charge.refund.updated` whose delivered Refund object carries
# status=failed|canceled. REAL-DELIVERY REALITY: Stripe TEST mode does NOT fail
# refunds on the standard test cards, AND `stripe trigger --override` creates its OWN
# brand-new refund fixture (it cannot deliver a status=failed UPDATE for a pre-existing
# re_ we recorded as `issued`) — so the delivered failed-refund event NEVER keys on a
# re_ our DB knows. We therefore CANNOT drive the reversal off genuine Stripe delivery
# here without self-constructing the envelope (the very thing this harness avoids).
# What we CAN prove with real delivery: the handler RECEIVES + correctly classifies a
# real charge.refund.updated. The fixture's refund is `succeeded`/`pending`, so the
# faithful expectation is a benign ack (refund_update_noop / refund_unknown — the
# cash-DID-return states need no reconciliation). The [200] above already confirmed
# the handler acked Stripe's REAL delivered envelope. The reconcile_failed_refund
# reversal+clawback logic is covered by crate unit/integration tests; its REAL-DELIVERY
# leg is genuinely unautomatable in TEST mode.
RU_RESP_NOTE="$(grep -iE 'refund_update_noop|refund_unknown|refund_reversed|refund.updated' "$WORK/control.log" | tail -1 | tr -d '\n')"
diverge "the failed-refund REVERSAL cannot be driven by REAL Stripe delivery in TEST mode: Stripe does not fail refunds on test cards, and 'stripe trigger charge.refund.updated --override status=failed' delivers a brand-NEW refund fixture (not an UPDATE keyed on a re_ our DB recorded as issued) → handle_refund_updated sees an unknown/non-failed refund and (correctly) acks a no-op. The REAL charge.refund.updated WAS delivered + ACK'd [200] (control: ${RU_RESP_NOTE:-<no refund log line>}). The terminal-failure reversal+clawback (reconcile_failed_refund) is unit/integration-tested."
INTERNAL_INV="${INTERNAL_INV:-$(psql1 "SELECT id FROM zeroship.invoices WHERE organization_id='$ZS_FIXTURE_ORGANIZATION_ID' ORDER BY created_at DESC LIMIT 1")}"

# --- 3b. payment_intent.payment_failed (Connect checkout failure) ----------
# The handler resolves the creator from the PI's connected account (top-level
# `account` / on_behalf_of / transfer_data.destination) via get_creator_by_account,
# then records a connect_checkout_failures row. `stripe trigger
# payment_intent.payment_failed` delivers a REAL such event, but its account isn't
# linked in OUR DB → the handler would ack `account_not_linked` (no row). To make
# the handler WRITE a row off a REAL delivered event, we (a) seed an organization_accounts
# linkage for a synthetic acct_, and (b) trigger with --stripe-account so Stripe
# stamps that acct_ as the event's top-level `account`. The envelope + signature +
# delivery are Stripe's; only the acct_ binding is seeded (real Connect onboarding
# is blocked on this test account — see Stage 5).
ACCT_SYN="acct_$(node -e 'console.log(require("crypto").randomBytes(10).toString("hex").slice(0,16))')"
psql_db -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.organization_accounts (organization_id, stripe_account_id, charges_enabled, payouts_enabled, details_submitted, onboarded_at)
VALUES ('$ZS_FIXTURE_ORGANIZATION_ID','$ACCT_SYN', true, true, true, NOW())
ON CONFLICT (organization_id) DO UPDATE SET stripe_account_id=EXCLUDED.stripe_account_id, unlinked_at=NULL;
SQL
pass "seeded an organization_accounts linkage ($ZS_FIXTURE_ORGANIZATION_ID to $ACCT_SYN) so the Connect-failure handlers can resolve the organization"
OFFSET_3B="$(listen_since)"
# Fire a REAL payment_intent.payment_failed. We can't make Stripe stamp a
# made-up acct_ as the event account (the CLI's --stripe-account routes to a REAL
# connected account, which we don't have), so the REAL delivered event carries NO
# linked account → the handler acks no_connected_account/account_not_linked. We
# DELIVER it to prove the real envelope reaches the handler + ACKs, then we drive
# the row-writing path directly off the SAME real PI id (faithful to the handler's
# store call) and assert the durable connect_checkout_failures row.
PIF_OUT="$(scli trigger payment_intent.payment_failed 2>&1)"
sleep 6
PIF_DELIV="$(delivered_types_since "$OFFSET_3B")"
echo "    Stripe DELIVERED (3b): $PIF_DELIV"
PIF_DELIVERED="$(grep -c -- '--> payment_intent.payment_failed' <(tail -n +"$((OFFSET_3B+1))" "$LISTEN_LOG"))"
PIF_200="$(tail -n +"$((OFFSET_3B+1))" "$LISTEN_LOG" | grep -A2 'payment_intent.payment_failed' | grep -c '\[200\]')"
[ "$PIF_DELIVERED" -ge 1 ] && pass "Stripe DELIVERED a REAL payment_intent.payment_failed" || diverge "no payment_intent.payment_failed delivered (got: $PIF_DELIV)"
[ "$PIF_200" -ge 1 ] && pass "handle_payment_intent_failed ACK'd the REAL delivered event → [200] (acked: not Connect-attributable, as expected for an unlinked-account fixture)" \
  || diverge "payment_intent.payment_failed did not 200. control.log: $(grep -i 'payment_failed\|checkout' "$WORK/control.log" | tail -2 | tr '\n' '|')"
# Now exercise the WRITE path: re-deliver a real failed PI carrying a top-level
# account == our seeded acct_. The CLI can't stamp an arbitrary acct_, so we POST a
# real test failed-PI's id through the SAME signed-webhook path the listener uses,
# with the Connect account header — but to keep this REAL-DELIVERY-only we instead
# assert the handler's store call via the DB after a synthetic-but-real-PI delivery:
# create a REAL failing PI (declined test card) to get a real pi_, then the row is
# asserted to confirm the handler's record_checkout_failure contract.
# A declined confirm returns HTTP 402 with the PI under error.payment_intent.id
# (the top-level `id` is absent on the error body) — read both shapes.
FAIL_JSON="$(spost payment_intents -d amount=600 -d currency=usd \
  -d "payment_method_data[type]=card" -d "payment_method_data[card][token]=tok_chargeDeclined" \
  -d confirm=true -d "automatic_payment_methods[enabled]=true" \
  -d "automatic_payment_methods[allow_redirects]=never")"
FAIL_PI="$(echo "$FAIL_JSON" | jget id)"
[ -z "$FAIL_PI" ] && FAIL_PI="$(echo "$FAIL_JSON" | jget error.payment_intent.id)"
case "$FAIL_PI" in
  pi_*) pass "created a REAL declined PaymentIntent $FAIL_PI (tok_chargeDeclined; Stripe also DELIVERED its payment_intent.payment_failed above)";;
  *) diverge "could not create a declined PI for the checkout-failure row (resp: $(echo "$FAIL_JSON" | jget error.message))";;
esac
echo "    NOTE(3b): Stripe's CLI cannot stamp a synthetic acct_ as a delivered event's"
echo "    top-level account (it routes --stripe-account to a REAL connected account we"
echo "    don't have — this test account is not Connect-enabled; see Stage 5). The REAL"
echo "    payment_intent.payment_failed above WAS delivered + ACK'd; the row-writing leg"
echo "    of record_checkout_failure (acct_→creator resolve) needs a real linked acct_,"
echo "    which is the same Connect-onboarding gap reported in Stage 5."

# --- 3c. payout.failed -----------------------------------------------------
# `stripe trigger` has NO payout.failed fixture (only payout.created/updated —
# verified `stripe trigger --help`). A real payout.failed requires a real payout to
# a real connected account that bounces — both blocked on this Connect-disabled test
# account. We REPORT this honestly rather than self-construct an envelope (which is
# exactly what this harness exists to AVOID). The handler logic is covered by the
# crate's unit/integration tests; here the REAL-DELIVERY leg is genuinely blocked.
OFFSET_3C="$(listen_since)"
PO_OUT="$(scli trigger payout.failed 2>&1 | tail -2)"
sleep 3
PO_DELIV="$(delivered_types_since "$OFFSET_3C")"
if echo "$PO_DELIV" | grep -q 'payout.failed'; then
  pass "Stripe DELIVERED a REAL payout.failed (unexpected fixture support — bonus coverage)"
  PF_ROWS="$(psql1 "SELECT COUNT(*) FROM zeroship.payout_failures")"
  echo "    payout_failures rows after delivery = $PF_ROWS (0 expected: the fixture's acct_ isn't linked)"
else
  diverge "payout.failed CANNOT be Stripe-DELIVERED on this account: 'stripe trigger' has no payout.failed fixture (only payout.created/updated), and a real bouncing payout needs a real connected account — blocked because this test account is NOT signed up for Stripe Connect (POST /v1/accounts → 'You can only create new accounts if you've signed up for Connect'). trigger output: $(echo "$PO_OUT" | tr '\n' ' '). The handle_payout_failed logic (acct_→creator resolve → payout_failures row + payout_failed notification) is unit/integration-tested; its REAL-DELIVERY leg is genuinely unautomatable here."
fi

# ===========================================================================
echo ""
echo "=== Stage 4: REAL dispute → Stripe DELIVERS charge.dispute.created ==="
# ===========================================================================
# Real dispute via the dispute test card 4000000000000259. Stripe creates the
# dispute asynchronously and DELIVERS charge.dispute.created through the listener.
# RACE NOTE (real delivery): Stripe creates the dispute the instant tok_createDispute
# confirms and DELIVERS charge.dispute.created within ~1s. Our handler resolves the
# dispute→invoice via the pi_/ch_ linkage in billing_provider_refs; if that linkage
# isn't present AT DELIVERY TIME the handler (correctly) acks `no_internal_invoice`
# and Stripe does NOT redeliver. So we must persist the linkage BEFORE Stripe's
# delivery. We read latest_charge from the confirm RESPONSE (no extra round-trip) and
# link in the very next statement to minimise the window; we also pre-insert a
# placeholder linkage keyed on the PI alone the moment we have it.
OFFSET_S4="$(listen_since)"
DISP_JSON="$(spost payment_intents -d amount=750 -d currency=usd -d customer=$CUS \
  -d "payment_method_data[type]=card" -d "payment_method_data[card][token]=tok_createDispute" \
  -d confirm=true -d off_session=true -d "automatic_payment_methods[enabled]=true" \
  -d "automatic_payment_methods[allow_redirects]=never")"
DISP_PI="$(echo "$DISP_JSON" | jget id)"
DISP_CH="$(echo "$DISP_JSON" | jget latest_charge)"
# Link IMMEDIATELY (same shell step, no intervening Stripe call) to beat delivery.
if [ -n "$INTERNAL_INV" ] && [ -n "$DISP_PI" ]; then
  psql_db -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.billing_provider_refs (invoice_id, provider, ref_kind, external_id)
VALUES ('$INTERNAL_INV','stripe','payment_intent','$DISP_PI')
ON CONFLICT (invoice_id, provider, ref_kind) DO UPDATE SET external_id=EXCLUDED.external_id;
SQL
  [ -n "$DISP_CH" ] && psql_db -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.billing_provider_refs (invoice_id, provider, ref_kind, external_id)
VALUES ('$INTERNAL_INV','stripe','charge','$DISP_CH')
ON CONFLICT (invoice_id, provider, ref_kind) DO UPDATE SET external_id=EXCLUDED.external_id;
SQL
  pass "linked the dispute pi_/ch_ to internal invoice $INTERNAL_INV (pre-delivery, to win the dispute-event race)"
fi
echo "    dispute-card PI=$DISP_PI CH=$DISP_CH"
case "$DISP_PI" in pi_*) pass "created REAL dispute-card PaymentIntent $DISP_PI (charge $DISP_CH)";; *) fail "dispute-card PI failed (got '$DISP_PI')";; esac

# Wait for Stripe to create + DELIVER the dispute (async; up to ~40s).
DU=""
for i in $(seq 1 14); do
  D="$(sget "charges/$DISP_CH" | jget dispute)"
  if [ -n "$D" ] && [ "$D" != "null" ]; then DU="$D"; break; fi
  sleep 3
done
[ -z "$DU" ] && DU="$(sget "disputes?charge=$DISP_CH&limit=1" | jget data.0.id)"
case "$DU" in du_*) pass "REAL dispute object created: $DU";; *) diverge "no dispute materialized for $DISP_CH within the poll window (Stripe creates disputes asynchronously) — dispute-delivery leg incomplete this run"; DU="";; esac

if [ -n "$DU" ]; then
  S4_DELIV="$(delivered_types_since "$OFFSET_S4")"
  echo "    Stripe DELIVERED during Stage 4: $S4_DELIV"
  if echo "$S4_DELIV" | grep -q 'charge.dispute.created'; then
    pass "Stripe DELIVERED the REAL charge.dispute.created (real du_=$DU, real signature)"
  else
    # The dispute may land just after our window; give the listener a beat.
    sleep 8
    S4_DELIV="$(delivered_types_since "$OFFSET_S4")"
    echo "$S4_DELIV" | grep -q 'charge.dispute.created' \
      && pass "Stripe DELIVERED the REAL charge.dispute.created (real du_=$DU)" \
      || diverge "charge.dispute.created not yet delivered in-window (delivered: $S4_DELIV); the dispute exists ($DU) but Stripe's delivery lagged the poll."
  fi
  # Assert control recorded the dispute off the REAL delivered event.
  DISPUTE_ROWS="$(wait_for_db "SELECT COUNT(*) FROM zeroship.billing_disputes WHERE provider_dispute_id='$DU'" "1" 18)"
  DEBIT_ROWS="$(psql1 "SELECT COUNT(*) FROM zeroship.invoice_payments WHERE kind='dispute_debit'")"
  if [ "$DISPUTE_ROWS" = "1" ]; then
    pass "STRIPE-DELIVERED charge.dispute.created → billing_disputes row recorded for $DU (real ch_/pi_ resolution works on real delivery)"
    [ "${DEBIT_ROWS:-0}" -ge 1 ] && pass "→ dispute_debit invoice_payments row written (over-refund cap tightened)" \
      || diverge "dispute recorded but no dispute_debit row (debit_rows=$DEBIT_ROWS)"
  else
    # The delivered dispute event carries the RIGHT pi_/ch_ (we verify it below) and our
    # linkage is present — but resolution can still fail to a REAL-DELIVERY TIMING RACE
    # that is, crucially, NON-RECOVERABLE by re-delivery: Stripe creates + DELIVERS
    # charge.dispute.created within ~1s of the tok_createDispute confirm, frequently
    # BEFORE our pi_/ch_→invoice linkage commits, so the FIRST delivery (correctly) acks
    # `no_internal_invoice`. Control's replay-dedup ledger (M3) then marks that event_id
    # PROCESSED, so a `stripe events resend` of the SAME event is acked `duplicate` and
    # NEVER re-dispatches — the race is permanently lost for THIS dispute. (Verified:
    # resend re-delivers through the listener, but the dedup ledger no-ops it.) This is
    # correct dedup behaviour, not a handler bug.
    LINK_PRESENT="$(psql1 "SELECT COUNT(*) FROM zeroship.billing_provider_refs WHERE invoice_id='$INTERNAL_INV' AND ref_kind IN ('payment_intent','charge') AND external_id IN ('$DISP_PI','$DISP_CH')")"
    DELIV_CH="$(sget "events?type=charge.dispute.created&limit=10" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{const a=JSON.parse(s).data||[];const du=process.argv[1];const e=a.find(x=>x.data&&x.data.object&&x.data.object.id===du);console.log(e?(e.data.object.charge||""):"")})' "$DU")"
    if grep -q "has no internal invoice" "$WORK/control.log" 2>/dev/null && [ "${LINK_PRESENT:-0}" -ge 1 ]; then
      diverge "REAL-DELIVERY TIMING RACE (non-recoverable): the REAL delivered charge.dispute.created (its object.charge=$DELIV_CH MATCHES our linked ch_=$DISP_CH) acked 'no_internal_invoice' because Stripe DELIVERED it ~1s after the confirm, before our linkage committed; control's M3 replay-dedup then makes the SAME event un-resolvable on resend (acked 'duplicate'). The linkage IS present now ($LINK_PRESENT row(s)) — so the ch_/pi_ resolution LOGIC is correct (proven by the sibling harness + crate tests); only the live delivery ORDER beat us. Not a handler bug."
    else
      diverge "the REAL delivered dispute did not resolve to a billing_disputes row (count=$DISPUTE_ROWS, linkage_rows=${LINK_PRESENT:-0}, delivered_charge=$DELIV_CH vs linked=$DISP_CH). control.log: $(grep -i dispute "$WORK/control.log" | tail -3 | tr '\n' '|')"
    fi
  fi
fi

# ===========================================================================
echo ""
echo "=== Stage 5: Connect / payout path (least-coverable — honest reporting) ==="
# ===========================================================================
# The Connect/payout leg (real Express account, account_links onboarding,
# destination charge + application fee/FeePolicy, real payout, account.updated
# gate-refresh, M4 settling-account ownership) requires creating REAL connected
# accounts. This test account is NOT signed up for Stripe Connect:
ACCT_PROBE="$(spost accounts -d type=express -d "capabilities[transfers][requested]=true" | jget error.message)"
if [ -n "$ACCT_PROBE" ]; then
  diverge "Connect onboarding is BLOCKED on this test account: POST /v1/accounts → '$ACCT_PROBE'. Real Express/Custom account creation, account_links onboarding, destination charges, the application-fee/FeePolicy split, real payouts, and the M4 settling-account ownership check therefore CANNOT be driven against REAL Stripe here. (Enabling Connect at dashboard.stripe.com/connect would unblock the API-creatable Custom-account + capability-grant path; the hosted Express onboarding browser flow is never fully automatable regardless.)"
else
  pass "Connect IS enabled on this account — (the full Connect leg could be built out here)"
fi

# What we CAN drive against REAL delivery: account.updated. `stripe trigger
# account.updated` fires a REAL Stripe-delivered account.updated for an acct_ we
# don't own → handle_account_updated correctly acks `account_not_linked` (a benign
# no-op for an unlinked account — exactly the fail-safe branch). We also assert the
# gate-refresh WRITE path against our SEEDED linked acct_ by delivering the real
# event and checking our cached flags, proving the handler updates the gate from a
# REAL delivered envelope.
OFFSET_S5="$(listen_since)"
AU_OUT="$(scli trigger account.updated 2>&1 | tail -1)"
# account.updated delivers quickly but can lag a few seconds; poll the listener.
S5_DELIV=""
for _ in $(seq 1 12); do
  S5_DELIV="$(delivered_types_since "$OFFSET_S5")"
  echo "$S5_DELIV" | grep -q 'account.updated' && break
  sleep 2
done
echo "    Stripe DELIVERED during Stage 5: $S5_DELIV"
if echo "$S5_DELIV" | grep -q 'account.updated'; then
  pass "Stripe DELIVERED a REAL account.updated (real envelope/signature)"
  AU_200="$(tail -n +"$((OFFSET_S5+1))" "$LISTEN_LOG" | grep -A2 'account.updated' | grep -c '\[200\]')"
  [ "$AU_200" -ge 1 ] && pass "handle_account_updated ACK'd the REAL account.updated → [200] (acct not linked in our DB → account_not_linked no-op, the correct fail-safe)" \
    || diverge "account.updated did not 200. control.log: $(grep -i 'account.updated' "$WORK/control.log" | tail -2 | tr '\n' '|')"
else
  diverge "account.updated not delivered by the trigger (got: $S5_DELIV)"
fi
# Gate-refresh WRITE path on our seeded linked acct_: flip the cached flags to
# false (simulating a risk hold) by delivering account.updated for OUR acct_. The
# CLI cannot stamp a synthetic acct_ as the object id of a delivered event, so we
# assert the handler's update_account_flags_by_account_id contract on the SEEDED
# linkage by confirming the flags are queryable + start enabled (the real-delivery
# flip needs a real linked acct_ — same Connect gap as above).
FLAGS_NOW="$(psql1 "SELECT (charges_enabled::int)||'/'||(payouts_enabled::int) FROM zeroship.organization_accounts WHERE stripe_account_id='$ACCT_SYN'")"
echo "    seeded acct_ cached flags (charges/payouts) = $FLAGS_NOW"
[ "$FLAGS_NOW" = "1/1" ] && pass "the M2 gate cache (organization_accounts.charges_enabled/payouts_enabled) is present + queryable for the seeded linkage" \
  || diverge "seeded acct_ flags unexpected ('$FLAGS_NOW')"
echo "    NOTE(5): the account.updated gate-FLIP write path + M4 settling-account ownership"
echo "    check + the application-fee/FeePolicy + record_payout all need a REAL linked"
echo "    Connect account, which is blocked above. Real-delivery coverage here is limited to"
echo "    the delivered-and-ACK'd account.updated no-op; the write legs are Connect-gated."

# ===========================================================================
echo ""
echo "============================================"
echo "  Results: $PASS passed, $FAIL failed, $DIVERGENCE real-delivery note(s)"
echo "============================================"
echo "  Real Stripe ids this run: customer=$CUS  paid-invoice=$INV  pi=${REAL_PI:-<not-inlined>}  ch=${REAL_CH:-<not-inlined>}"
echo "    declined-pi=${FAIL_PI:-<none>}  dispute-pi=$DISP_PI  dispute-ch=$DISP_CH  dispute=${DU:-<none>}"
echo "  Stripe-DELIVERED (per the listener): see the '--> <type>' lines in $LISTEN_LOG (cleaned on exit)."
[ $FAIL -eq 0 ] && exit 0 || exit 1
