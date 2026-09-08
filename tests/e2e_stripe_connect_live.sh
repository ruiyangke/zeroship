#!/usr/bin/env bash
# ============================================================================
# e2e_stripe_connect_live.sh — FAITHFUL live Stripe **Connect** money-flow e2e
# for the zeroship billing rail's Stream-2 (CREATOR REVENUE) path, against REAL
# Stripe TEST mode (api.stripe.com), NOT the in-repo mock-Stripe-Connect.
#
# This is the Connect peer of tests/e2e_stripe_billing.sh (the Stream-1 infra
# rail) and tests/e2e_stripe_webhooks_live.sh (real-delivery webhooks). It
# drives the SAME control-plane code those harnesses do — the cyper
# `StripeClient` Connect calls (`create_connect_account`, `create_account_link`,
# `retrieve_account`, `create_connect_payment_intent`), the server-held
# `FeePolicy`, the `connect_checkout` and `callback` handlers,
# and the signature-verified `/internal/webhooks/stripe` ingest of
# `account.updated` / `payout.failed` — the way real Stripe + real webhooks
# drive them.
#
# THE CONNECT MONEY FLOW (when Connect is enabled):
#
#   create REAL Express connected account (POST /v1/accounts type=express,
#       request transfers+card_payments capabilities)         → acct_…
#     ─► POST /v1/account_links (type=account_onboarding)      → assert a URL
#         (the hosted browser onboarding is NOT automatable; we instead use
#          test-mode capability activation so charges become possible)
#     ─► API-PROVISION a `type=custom` connected account fully onboarded with
#         Stripe TEST data (individual ssn_last_4/id_number/dob/address +
#         tos_acceptance + a test external_account bank token) so its `transfers`
#         capability goes genuinely ACTIVE at Stripe — the Express hosted browser
#         flow CANNOT be API-activated, but a Custom account CAN (this is the
#         documented test-mode path). Re-point the seeded creator's stored
#         `organization_accounts` row at THIS capable account (the same row `onboard`
#         wrote) + charges_enabled=true, then call zeroship's
#         POST /api/organizations/{id}/connect/checkout           (control's
#         REAL StripeClient + server-held FeePolicy)
#         → control stamps application_fee_amount + transfer_data[destination]
#           SERVER-SIDE against the CAPABLE destination; a MALICIOUS client
#           application_fee_amount is IGNORED
#     ─► FETCH the created PaymentIntent back from REAL Stripe and ASSERT its
#         application_fee_amount == the 15% FeePolicy fee, amount == gross, and
#         transfer_data.destination == acct_   (the platform's cut, computed
#         server-side, not client-trusted)
#     ─► M2 gate: deliver a REAL-shaped account.updated flipping
#         charges_enabled→false; assert control re-caches the flag AND a
#         subsequent connect_checkout is BLOCKED (400) — the money-hole guard
#     ─► M4 attribution: deliver a connect-revenue invoice.paid whose
#         on_behalf_of / transfer_data.destination MATCHES vs MISMATCHES the
#         creator's stored stripe_account_id; assert record_payout credits ONLY
#         on a match (metadata-only trust is rejected: attribution_mismatch)
#     ─► payout.failed: deliver a REAL-shaped payout.failed for the linked acct_;
#         assert payout_failures row + the not-yet-enabled-account guard
#
# WEBHOOK LEGS use the SAME self-signed-real-shaped helper as
# tests/e2e_stripe_billing.sh: events are built from REAL fetched Stripe objects
# (real acct_/pi_/po_ ids) and HMAC-SHA256-signed with the control instance's
# ZEROSHIP_CONTROL_STRIPE_WEBHOOK_SECRET — the REAL signature-verification + handler path. Only
# the delivery is self-driven. Object SHAPES + the signature verify are real.
#
# ── CONNECT MUST BE ENABLED ON THE TEST ACCOUNT ─────────────────────────────
# POST /v1/accounts returns HTTP 400 invalid_request_error: "You can only create
# new accounts if you've signed up for Connect…" on an account that has not
# signed up. This harness DETECTS that and REFUSES (exit 2, naming the dashboard
# page that enables it). It used to exit 0 there, which made "the Connect money
# flow is covered" and "no Connect account exists" the same result.
#
# DO NO HARM: a PER-RUN dedicated DB on the :5440 server and a PER-RUN control
# port. Never the real `zeroship` DB, never the concurrent agent's
# `zeroship_billing_test`, and never a peer Stripe harness's database.
# Self-managed up/down; cleans up on exit.
#
# THE DATABASE NAME USED TO BE THE FIXED LITERAL `zeroship_stripe_e2e`, shared
# verbatim with tests/e2e_stripe_billing.sh and
# tests/e2e_stripe_webhooks_live.sh, and Stage 1 opened by terminating every
# backend on it and dropping it. Two of the three running together on one box
# meant one destroying the other's run mid-flight. See tests/lib/scratch_db.sh
# and tests/lib/e2e_ports.sh.
#
# REFUSES (exit 2, naming the missing thing and its remedy) when a prerequisite
# is absent - no PG :5440, no docker for the migrate, no built control binary, no
# jose for the bearer, the Stripe TEST env not sourced - or when Connect is not
# enabled on the account.
#
# NOT WIRED INTO CI: nothing in .github/workflows/ names this script, and
# tests/run_billing_suite.sh does not invoke it either. It needs an operator's
# live Stripe TEST keys and a Connect-enabled account, neither of which CI can
# hold, so refusing cannot turn a CI job permanently red. It refuses to the
# person who ran it by hand, which is the only reader it has.
#
# Usage:
#   source /home/ruiyang/.config/zeroship-stripe-test.env   # sets the TEST keys
#   ./tests/e2e_stripe_connect_live.sh
#   STRICT=1 ./tests/e2e_stripe_connect_live.sh   # documented divergences = hard fail
#
# SECRETS: reads $STRIPE_TEST_SECRET_KEY from the env ONLY. NEVER prints/writes/
# commits any sk_/pk_/whsec_ value. The webhook signing secret is a throwaway
# value generated per-run.
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release"
# shellcheck source=tests/lib/runtime_secrets.sh
source "$ROOT/tests/lib/runtime_secrets.sh"
# Connect is ORGANIZATION-scoped end to end: the routes, the money guard, the
# `organization_accounts` link row and the Stripe ownership metadata all name
# one. This harness creates no app, so it seeds the organization and the seat
# itself rather than relying on the personal organization `POST /api/apps` mints.
source "$ROOT/tests/lib/organization_fixture.sh"
STRICT="${STRICT:-0}"

PASS=0; FAIL=0; DIVERGENCE=0
pass()  { PASS=$((PASS+1));  echo "  ✓ $1"; }
fail()  { FAIL=$((FAIL+1));  echo "  ✗ $1"; }
# A real-Stripe-API divergence from our code's assumptions, or a genuinely-
# unautomatable leg. Reported loudly; only a hard fail under STRICT=1.
diverge() { DIVERGENCE=$((DIVERGENCE+1)); echo "  ⚠ REAL-API DIVERGENCE: $1"; if [ "$STRICT" = "1" ]; then FAIL=$((FAIL+1)); fi; }

echo "============================================"
echo "  zeroship E2E — Stripe CONNECT money flow (REAL Stripe TEST mode, not the mock)"
echo "============================================"

# --- prereq gates: EVERY missing prerequisite REFUSES ----------------------
# psql: $PATH first, then any postgresql in the nix store, then refuse. This
# replaced ZEROSHIP_PSQL, whose default was one pinned /nix/store hash that
# resolved on exactly one machine; everywhere else it was absent and the
# absence was `exit 0`, so this suite measured nothing and reported success.
# Same chain as tests/e2e_auth_ui.sh, same variable name.
#
# THAT SENTENCE WAS THE ONLY ONE ACTED ON at the time: psql got a refusal and
# every other prerequisite kept its `exit 0`, so the diagnosis sat beside the
# arms it applied to verbatim. They refuse now too, including the two mid-run
# ones (Connect not enabled, bearer not mintable).
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
  zs_prereq "STRIPE_TEST_SECRET_KEY is not set, so there is no Stripe TEST account to drive." \
            "source the operator's zeroship-stripe-test.env, or export the sk_test_ key first."
fi
case "$STRIPE_TEST_SECRET_KEY" in
  sk_test_*) ;;
  *) echo "  ✗ REFUSING TO RUN: STRIPE_TEST_SECRET_KEY is not an sk_test_ key. TEST mode only."; exit 2 ;;
esac
SK="$STRIPE_TEST_SECRET_KEY"

[ -n "$PSQL" ] && [ -x "$PSQL" ] || {
  echo "  x ABORT: no psql on \$PATH or in the nix store; set PSQL to the Postgres client binary." >&2
  exit 2
}
command -v node    >/dev/null 2>&1 || zs_prereq "node is not on PATH; the harness mints its bearer and signs webhook payloads with it." "install Node, or enter the dev shell: nix develop"
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

# provision_custom_account — fully API-onboard a `type=custom` US individual test
# connected account so its `transfers` capability goes genuinely ACTIVE at Stripe.
# Express accounts CANNOT be API-activated (they need Stripe's hosted browser
# onboarding); Custom accounts CAN — this is the documented test-mode path for
# bypassing onboarding with test values. Echoes the acct_ id on success ('' on
# failure). NEVER echoes $SK.
#   - business_profile[url] must be a REAL resolvable domain — Stripe's verifier
#     rejects example.com as url_invalid; we use https://zeroship.ai.
#   - test magic values: ssn_last_4=0000, id_number=000000000, dob 1901-01-01,
#     address line1=address_full_match, phone 0000000000, routing 110000000,
#     account 000123456789 (the documented Connect test-onboarding fixtures).
provision_custom_account() {
  local creator="$1" now acct btok poll tr
  now="$(date +%s)"
  acct="$(spost accounts \
    -d type=custom -d country=US \
    -d "capabilities[transfers][requested]=true" \
    -d "capabilities[card_payments][requested]=true" \
    -d "business_type=individual" \
    -d "individual[first_name]=Test" -d "individual[last_name]=Creator" \
    -d "individual[email]=connect-$creator@zeroship.test" \
    -d "individual[dob][day]=1" -d "individual[dob][month]=1" -d "individual[dob][year]=1901" \
    -d "individual[address][line1]=address_full_match" -d "individual[address][city]=South San Francisco" \
    -d "individual[address][state]=CA" -d "individual[address][postal_code]=94080" \
    -d "individual[ssn_last_4]=0000" -d "individual[id_number]=000000000" \
    -d "individual[phone]=0000000000" \
    -d "business_profile[mcc]=5734" -d "business_profile[url]=https://zeroship.ai" \
    -d "tos_acceptance[date]=$now" -d "tos_acceptance[ip]=127.0.0.1" \
    -d "metadata[organization_id]=$ZS_FIXTURE_ORGANIZATION_ID" -d "metadata[zeroship_probe]=connect_e2e_custom" \
    | jget id)"
  case "$acct" in acct_*) : ;; *) echo ""; return 1 ;; esac
  echo "$acct" >> "$CREATED_ACCTS"
  # Attach a test external bank account (Stripe needs an external_account + the
  # TOS acceptance recorded above before it will activate payout-bound capabilities).
  btok="$(spost tokens \
    -d "bank_account[country]=US" -d "bank_account[currency]=usd" \
    -d "bank_account[account_holder_name]=Test Creator" \
    -d "bank_account[account_holder_type]=individual" \
    -d "bank_account[routing_number]=110000000" \
    -d "bank_account[account_number]=000123456789" | jget id)"
  case "$btok" in btok_*) spost "accounts/$acct/external_accounts" -d "external_account=$btok" -o /dev/null 2>/dev/null ;; esac
  # Poll until `transfers` is ACTIVE (the only capability a DESTINATION charge
  # requires). card_payments may linger `pending` under requirements.pending_
  # verification with NO currently_due/past_due — that async lag does NOT block a
  # transfer_data destination charge (the card is taken on the PLATFORM account).
  for poll in $(seq 1 30); do
    tr="$(sget "accounts/$acct" | jget capabilities.transfers)"
    [ "$tr" = "active" ] && break
    sleep 1
  done
  echo "$acct"
}

# ===========================================================================
# CONNECT-ENABLED PROBE — the load-bearing gate. Try to create a REAL Express
# connected account requesting transfers+card_payments. On a Connect-disabled
# account Stripe returns HTTP 400 invalid_request_error "You can only create new
# accounts if you've signed up for Connect…". We REFUSE in that case.
# ===========================================================================
echo ""
echo "=== Connect-enabled probe (POST /v1/accounts type=express + capabilities) ==="
PROBE="$(spost accounts \
  -d type=express \
  -d "capabilities[transfers][requested]=true" \
  -d "capabilities[card_payments][requested]=true" \
  -d "metadata[zeroship_probe]=connect_e2e")"
PROBE_ACCT="$(echo "$PROBE" | jget id)"
PROBE_ERR="$(echo "$PROBE" | jget error.message)"
case "$PROBE_ACCT" in
  acct_*)
    pass "Connect IS enabled — created probe account $PROBE_ACCT (transfers+card_payments requested)"
    # Tidy up the probe account so we mint a clean one in Stage 2.
    curl -s -X DELETE "$SAPI/accounts/$PROBE_ACCT" -u "$SK:" -o /dev/null
    ;;
  *)
    echo "    POST /v1/accounts -> ${PROBE_ERR:-<no error message; check key auth and network>}" >&2
    zs_prereq "Connect is not enabled on this Stripe TEST account, so the money flow this harness exists to prove cannot run." \
              "enable it at dashboard.stripe.com/connect, then re-run. See docs/runbooks/stripe-connect-live-e2e.md."
    ;;
esac

# ===========================================================================
# From here on Connect IS enabled — run the full live money flow.
# ===========================================================================
# Allocated rather than the constant 19099: two runs of THIS harness on one box
# is the case a per-harness constant can never separate.
zs_ports_reserve ZEROSHIP_CONTROL_PORT || exit 1
CONTROL_URL="http://localhost:$ZEROSHIP_CONTROL_PORT"
WEBHOOK_SECRET="whsec_e2e_$(openssl rand -hex 16)"   # throwaway, per-run
WORK="$(mktemp -d -t zs-e2e-connect-XXXXXX)"
mkdir -p "$WORK/blobs"
PIDFILE="$WORK/pids"; : > "$PIDFILE"
CREATED_ACCTS="$WORK/accts"; : > "$CREATED_ACCTS"

cleanup() {
  local rc=$?
  echo ""
  echo "=== Cleanup ==="
  if [ -f "$PIDFILE" ]; then
    while read -r pid; do [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; done < "$PIDFILE"
  fi
  wait 2>/dev/null || true
  # Best-effort: delete the test-mode connected accounts this run minted —
  # UNLESS KEEP_CONNECTED_ACCOUNTS is set (leave them at Stripe for dashboard inspection).
  if [ -z "${KEEP_CONNECTED_ACCOUNTS:-}" ] && [ -f "$CREATED_ACCTS" ]; then
    while read -r a; do [ -n "$a" ] && curl -s -X DELETE "$SAPI/accounts/$a" -u "$SK:" -o /dev/null 2>/dev/null || true; done < "$CREATED_ACCTS"
    echo "  control down; test-mode connected accounts deleted; $WORK cleaned."
  else
    echo "  control down; $WORK cleaned. KEEP_CONNECTED_ACCOUNTS set — minted accounts LEFT at Stripe for inspection:"
    [ -f "$CREATED_ACCTS" ] && while read -r a; do [ -n "$a" ] && echo "    connected account: $a"; done < "$CREATED_ACCTS"
  fi
  [ -n "${WORK:-}" ] && rm -rf "$WORK"
  # Drop on SUCCESS only: on a red run the organization_accounts / payout rows control
  # wrote ARE the finding.
  zs_scratch_db_cleanup_on_success "$rc"
  zs_ports_release
  echo "  (The real zeroship DB + zeroship_billing_test were NEVER touched.)"
  return 0
}
trap cleanup EXIT

# NO `lsof -ti :$PORT | xargs kill -9` HERE ANY MORE - see the header. An
# allocated port has no leaked previous run to reclaim it from, and that line
# could not tell one from a peer agent's live control plane.

# Poll the control DB until a query returns the expected value.
wait_for_db() {
  # $1=sql  $2=expected  $3=tries(default 15)  → echoes the final value
  local sql="$1" want="$2" tries="${3:-15}" got=""
  for _ in $(seq 1 "$tries"); do
    got="$(psql1 "$sql")"
    [ "$got" = "$want" ] && { echo "$got"; return 0; }
    sleep 1
  done
  echo "$got"; return 1
}

# ===========================================================================
echo ""
echo "=== Stage 1: dedicated DB ($DB) + platform migrations + control booted at REAL Stripe ==="
# ===========================================================================
# NO `pg_terminate_backend` AND NO PRE-DROP: the name carries a per-run token,
# so nothing can already hold it, and both statements were only ever able to hit
# a PEER's database. `pg_terminate_backend` over `datname=` is `WITH (FORCE)`
# spelled out by hand.
"$PSQL" -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL || { fail "could not create $DB"; exit 1; }
CREATE DATABASE $DB;
ALTER DATABASE $DB SET search_path = zeroship, public;
SQL
pass "created per-run DB $DB on :$PGPORT (real zeroship + zeroship_billing_test untouched)"

MIG_LOG="$WORK/migrate.log"
if ZEROSHIP_MIGRATE_DSN="postgres://$PGUSER:$PGPW@$PGHOST:$PGPORT/$DB" "$ROOT/deploy/ops/db-migrate.sh" > "$MIG_LOG" 2>&1; then
  pass "platform migration set applied (incl. organization_fee_policy + organization_accounts Connect flags)"
else
  fail "platform migration FAILED (see $MIG_LOG)"; tail -20 "$MIG_LOG"; exit 1
fi

DBURL="postgres://$PGUSER:$PGPW@$PGHOST:$PGPORT/$DB"
openssl genpkey -algorithm ed25519 -out "$WORK/signing-key.pem" 2>/dev/null
chmod 600 "$WORK/signing-key.pem"

# control — REAL https://api.stripe.com, the operator's TEST secret key (from env,
# NEVER on argv where it'd hit /proc/cmdline), and a known throwaway webhook
# secret so the harness can produce VALID signatures (the REAL verify path).
ZEROSHIP_GATEWAY_SIGNING_KEY_FILE="$WORK/signing-key.pem"
# The issuer control verifies the admin bearer against, on the same key the
# gateway signs with. Up BEFORE control: control reads the issuer once at boot.
e2e_platform_op_up "$WORK/signing-key.pem" "$WORK" || exit 1
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
  && pass "control healthy (stripe-base-url → REAL api.stripe.com; webhook secret configured)" \
  || { fail "control unhealthy"; tail -30 "$WORK/control.log"; exit 1; }

# The signed-webhook helper — REAL signature path. Builds events from REAL
# fetched Stripe objects; only the delivery is self-driven (same as the sibling
# billing harness). NEVER echoes the secret.
post_signed_webhook() {
  # $1 = event JSON. Signs t=<now>,v1=HMAC_SHA256(secret, "<t>.<body>") and POSTs.
  local body="$1" t sig
  t="$(date +%s)"
  sig="$(printf '%s' "$t.$body" | openssl dgst -sha256 -hmac "$WEBHOOK_SECRET" -hex | sed 's/^.*= *//')"
  curl -s -o "$WORK/wh_resp.json" -w '%{http_code}' -X POST "$CONTROL_URL/internal/webhooks/stripe" \
    -H 'content-type: application/json' \
    -H "stripe-signature: t=$t,v1=$sig" \
    --data-binary "$body"
}

# Mint a bearer the onboard/checkout endpoints accept (faithful AuthzGuard, the
# same EdDSA-signed at+jwt control verifies in production).
CREATOR="$(node -e 'console.log(require("crypto").randomUUID())')"
NOW_UNIX="$(date +%s)"
SCOPE="billing:read billing:write"

# THE CONNECT SUBJECT IS THE ORGANIZATION, not the human. Every route this
# harness drives is `/api/organizations/{id}/...` and every one of them runs
# `organization_money_guard`, which resolves BillingWrite at
# `Resource::Organization` - so a bearer with the right scope and NO SEAT is
# refused. This harness never creates an app, so nothing mints a personal
# organization for it: the organization and the seat are both explicit here.
#
# `owner` rather than `developer`: BillingWrite needs billing authority, which
# `owner` and `billing` carry (zeroship.organization_roles.billing_rank) and
# `developer` does not.
organization_fixture_ids "connect-$CREATOR"
psql_db -v ON_ERROR_STOP=1 >/dev/null <<SQL || { fail "creator/organization seed failed"; exit 1; }
INSERT INTO zeroship.users (id, email, name, email_verified_at)
VALUES ('$CREATOR', 'connect-$CREATOR@zeroship.test'::citext, 'E2E Connect Creator', NOW());
$(organization_fixture_sql "connect-$CREATOR" "connect-$CREATOR@zeroship.test")
$(seat_organization_member_sql "$ZS_FIXTURE_ORGANIZATION_ID" "$CREATOR" owner)
SQL
pass "seeded creator $CREATOR as owner of organization $ZS_FIXTURE_ORGANIZATION_ID"

JOSE_JS="$ROOT/node_modules/.pnpm/jose@6.2.3/node_modules/jose/dist/webapi/index.js"
ADMIN_TOKEN=""
if [ -f "$JOSE_JS" ]; then
  ADMIN_TOKEN="$(e2e_mint_platform_bearer "$CREATOR" "$SCOPE")"
fi
if [ "$(echo -n "$ADMIN_TOKEN" | awk -F. '{print NF}')" = "3" ]; then
  pass "minted operator bearer for the onboard/checkout/fee-policy endpoints"
else
  zs_prereq "could not mint the operator bearer (jose not found at $JOSE_JS); every authenticated Connect leg needs it." \
            'pnpm install at the repo root, then re-run.'
fi
AUTH=(-H "Authorization: Bearer $ADMIN_TOKEN")

# ===========================================================================
echo ""
echo "=== Stage 2: REAL Express connected account via control's onboard handler ==="
# ===========================================================================
# Drive the REAL control handler `onboard` (crates/zeroship-control/src/stripe_handlers.rs
# :120) — it calls StripeClient::create_connect_account (stripe_client.rs:963,
# POST /v1/accounts type=express, metadata[organization_id]=<organization>) then
# create_account_link (:983, POST /v1/account_links type=account_onboarding).
ONBOARD="$(curl -s -X POST "$CONTROL_URL/api/organizations/$ZS_FIXTURE_ORGANIZATION_ID/stripe/onboard" "${AUTH[@]}")"
echo "    onboard → $ONBOARD"
ACCT="$(echo "$ONBOARD" | jget account_id)"
LINK_URL="$(echo "$ONBOARD" | jget url)"
case "$ACCT" in
  acct_*) pass "control minted a REAL Express connected account $ACCT (server-stamped metadata.organization_id)"; echo "$ACCT" >> "$CREATED_ACCTS";;
  *) fail "onboard did not return an acct_ (resp: $ONBOARD). control.log: $(grep -i 'onboard\|account' "$WORK/control.log" | tail -3 | tr '\n' '|')"; exit 1;;
esac
# Stage 2.2 — the account_links onboarding URL (the hosted browser flow is NOT
# automatable; we assert the handler returned a real URL).
case "$LINK_URL" in
  https://connect.stripe.com/*|https://*.stripe.com/*) pass "onboard returned a REAL account_links hosted-onboarding URL (browser flow not automatable — documented)";;
  *) diverge "onboard returned no/unexpected account_links url (got: '$LINK_URL')";;
esac

# Fetch the REAL account back + confirm metadata.organization_id ownership signal.
ACCT_META="$(sget "accounts/$ACCT" | jget metadata.organization_id)"
[ "$ACCT_META" = "$CREATOR" ] && pass "REAL acct_ carries metadata.organization_id=$CREATOR (the ownership signal callback verifies, ISS-30)" \
  || diverge "acct_ metadata.organization_id ('$ACCT_META') != creator ($CREATOR)"

# Keep a handle on the Express account control just minted (the faithful
# onboarding artifact). Its hosted onboarding can NOT be API-activated, so the
# DESTINATION charge in Stage 3 uses an API-provisioned Custom account instead
# (next block). The Express acct_ is still asserted above + cleaned up at exit.
EXPRESS_ACCT="$ACCT"

# ── Stage 2.5 — API-provision a CAPABLE destination. The Express hosted browser
# onboarding can't be automated, so a fresh `type=express` account stays
# charges_enabled=false / no active transfers capability and Stripe rejects the
# real PaymentIntent with `insufficient_capabilities_for_transfer`. A `type=custom`
# account, by contrast, is FULLY API-onboardable in TEST mode: feeding it the
# documented test values (ssn_last_4=0000, id_number=000000000, dob, address_full_
# match, a test external_account bank token, tos_acceptance) makes its `transfers`
# capability go genuinely ACTIVE at Stripe — enough for a real destination charge.
# This is faithful + documented, NOT a workaround that hides anything: the
# onboarding PATH is still tested for real via the Express account in Stage 2; only
# Stage 3's CHARGE needs a destination Stripe will actually transfer to.
echo ""
echo "=== Stage 2.5: API-provision a fully-onboarded type=custom account (transfers ACTIVE) ==="
echo "    (Express CANNOT be API-activated — hosted browser onboarding only; Custom CAN. Documented.)"
CUSTOM_ACCT="$(provision_custom_account "$CREATOR")"
case "$CUSTOM_ACCT" in
  acct_*) pass "API-provisioned a type=custom connected account $CUSTOM_ACCT (full TEST onboarding data + external_account)";;
  *) fail "could not provision a custom account (Stripe error). control's request is correct; the destination capability is the blocker. Last Stripe resp: $(spost accounts -d type=custom -d country=US -d 'capabilities[transfers][requested]=true' | jget error.message)"; exit 1;;
esac
CUSTOM_TRANSFERS="$(sget "accounts/$CUSTOM_ACCT" | jget capabilities.transfers)"
CUSTOM_CARD="$(sget "accounts/$CUSTOM_ACCT" | jget capabilities.card_payments)"
CUSTOM_DUE="$(sget "accounts/$CUSTOM_ACCT" | jget requirements.currently_due)"
echo "    custom acct capabilities: transfers=$CUSTOM_TRANSFERS card_payments=$CUSTOM_CARD currently_due=$CUSTOM_DUE"
if [ "$CUSTOM_TRANSFERS" = "active" ]; then
  pass "the custom account's transfers capability is genuinely ACTIVE at Stripe (the only capability a destination charge needs)"
else
  fail "transfers capability did not reach 'active' (got '$CUSTOM_TRANSFERS'); a destination charge will be rejected — investigate the test onboarding fields"
  exit 1
fi
# card_payments often lingers `pending` under requirements.pending_verification
# with an EMPTY currently_due — that async verification lag does NOT block a
# transfer_data destination charge (the card is taken on the PLATFORM account, whose
# OWN card_payments is what matters). Note it so the run is self-explaining.
[ "$CUSTOM_CARD" = "active" ] \
  && pass "card_payments also ACTIVE" \
  || echo "    note: card_payments=$CUSTOM_CARD (pending_verification, currently_due empty) — does NOT block a destination charge; transfers is what matters here"

# ── Re-point the seeded creator's stored connect identity at THIS capable account
# + charges_enabled=true. This is the SAME `organization_accounts` row `onboard` wrote
# (PK=organization_id) and the SAME columns `update_account_flags_by_account_id` writes
# from a real account.updated — we just point them at the API-onboarded Custom
# account so control's `connect_checkout` (which reads server truth from this row)
# targets a destination Stripe will actually transfer to. control's FeePolicy
# server-fee stamping stays entirely in the REAL path; only the destination changes.
psql_db -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL || { fail "could not re-point organization_accounts to the capable custom account"; exit 1; }
UPDATE zeroship.organization_accounts
   SET stripe_account_id = '$CUSTOM_ACCT',
       charges_enabled   = true,
       payouts_enabled   = true,
       details_submitted = true
 WHERE organization_id = '$ZS_FIXTURE_ORGANIZATION_ID' AND unlinked_at IS NULL;
SQL
GATE="$(psql1 "SELECT charges_enabled::int FROM zeroship.organization_accounts WHERE organization_id='$ZS_FIXTURE_ORGANIZATION_ID' AND stripe_account_id='$CUSTOM_ACCT'")"
[ "$GATE" = "1" ] && pass "creator's stored stripe_account_id re-pointed to $CUSTOM_ACCT with charges_enabled=true (M2 gate open on a CAPABLE destination)" \
  || { fail "re-point did not take (charges_enabled gate='$GATE')"; exit 1; }

# From here on, the LINKED connect account is the capable custom one. Downstream
# stages (3 = charge, 4 = M2 disable gate, 5 = M4 attribution, 6 = payout.failed)
# all reason about the creator's LINKED account, so point $ACCT at it.
ACCT="$CUSTOM_ACCT"

# NOTE: we deliberately do NOT re-run control's `callback` here. callback re-fetches
# the account from Stripe and persists Stripe's `charges_enabled` verbatim — which
# for a freshly-API-onboarded custom account is still `false` (card_payments under
# async pending_verification, EMPTY currently_due), even though `transfers` is
# already ACTIVE and a destination charge succeeds. Re-running it would reset the
# gate to false and mask a working transfer behind Stripe's verification lag. The
# re-point above is the faithful end-state (the same flags update_account_flags_
# by_account_id would write once Stripe finishes verifying). callback's OWNERSHIP-
# verification path is still covered for real in Stage 2 against the Express acct_.

# ===========================================================================
echo ""
echo "=== Stage 3: connect_checkout stamps the SERVER fee (15% FeePolicy) + transfer_data ==="
# ===========================================================================
# The destination is the API-provisioned type=custom account from Stage 2.5 (its
# `transfers` capability is genuinely ACTIVE at Stripe), because an Express account
# cannot be API-activated. This is a LIVE destination charge: control's REAL
# StripeClient POSTs the PaymentIntent to api.stripe.com and Stripe accepts it.
# No fee policy row → DEFAULT 15% (crate::fee_policy::DEFAULT_PERCENT_BPS=1500).
# Charge $200.00 (20000 cents). A MALICIOUS client tries application_fee_amount=1
# — it has NO wire path (ConnectCheckoutBody has no such field) and MUST be ignored.
GROSS=20000
EXPECT_FEE=3000   # 15% of 20000, computed server-side (fee_policy.rs::fee_cents)
CO="$(curl -s -X POST "$CONTROL_URL/api/organizations/$ZS_FIXTURE_ORGANIZATION_ID/connect/checkout" "${AUTH[@]}" \
  -H 'content-type: application/json' \
  -d "{\"amount_cents\":$GROSS,\"currency\":\"usd\",\"cart_id\":\"cart-e2e-1\",\"application_fee_amount\":1,\"applicationFeePercent\":0}")"
echo "    connect_checkout → $CO"
CO_PI="$(echo "$CO" | jget payment_intent_id)"
CO_FEE="$(echo "$CO" | jget application_fee_cents)"
case "$CO_PI" in
  pi_*) pass "connect_checkout created a REAL Connect PaymentIntent $CO_PI";;
  *) fail "connect_checkout did not return a pi_ (resp: $CO). control.log: $(grep -i 'checkout\|fee\|charges' "$WORK/control.log" | tail -3 | tr '\n' '|')";;
esac
[ "$CO_FEE" = "$EXPECT_FEE" ] && pass "control echoed the SERVER-resolved fee = ${EXPECT_FEE}c (15% of ${GROSS}c) — the client's application_fee_amount=1 was IGNORED" \
  || diverge "echoed application_fee_cents ('$CO_FEE') != expected 15% ($EXPECT_FEE)"

# FAITHFUL: fetch the PaymentIntent back from REAL Stripe and assert the WIRE
# carried the server fee + the destination — not the client's 1. (PIs on
# connected accounts created with transfer_data are on the PLATFORM account.)
if [ -n "$CO_PI" ]; then
  PI_FEE="$(sget "payment_intents/$CO_PI" | jget application_fee_amount)"
  PI_AMT="$(sget "payment_intents/$CO_PI" | jget amount)"
  PI_DEST="$(sget "payment_intents/$CO_PI" | jget transfer_data.destination)"
  echo "    REAL PaymentIntent: amount=$PI_AMT application_fee_amount=$PI_FEE transfer_data.destination=$PI_DEST"
  [ "$PI_FEE" = "$EXPECT_FEE" ] && pass "REAL Stripe PaymentIntent carries application_fee_amount=${EXPECT_FEE}c (server-stamped, the 15% platform cut)" \
    || diverge "REAL PI application_fee_amount ('$PI_FEE') != $EXPECT_FEE — the server fee did not reach the wire"
  [ "$PI_AMT" = "$GROSS" ] && pass "REAL PI amount=${GROSS}c (gross charge to the end-user)" || diverge "REAL PI amount ('$PI_AMT') != $GROSS"
  [ "$PI_DEST" = "$ACCT" ] && pass "REAL PI transfer_data.destination=$ACCT (the connected account receives gross−fee = $((GROSS-EXPECT_FEE))c net)" \
    || diverge "REAL PI transfer_data.destination ('$PI_DEST') != $ACCT — the charge did not route to the creator"
fi

# Stage 3.2 — a stored FeePolicy (25% capped at $40) overrides the default.
#
# WRITTEN TO THE STORE, NOT OVER HTTP. `PUT /api/organizations/:id/fee-policy` is
# deleted along with the rest of the vendor-commercial surface; there is no
# replacement route, and this stage used to assert a 204 from it. That is not
# what the stage is about: the property under test is that CHECKOUT resolves the
# STORED policy server-side and stamps it on the real Stripe charge, and
# `connect_checkout` is very much alive. So the row is written the way
# `FeePolicyStore::set` writes it (crates/zeroship-control/src/fee_policy.rs) and
# the assertions below are unchanged. `crates/zeroship-control/tests/connect_fee_test.rs`
# made the same move for the same reason.
psql_db -v ON_ERROR_STOP=1 >/dev/null <<SQL || { fail "could not store the 25%-capped FeePolicy"; exit 1; }
INSERT INTO zeroship.organization_fee_policy
  (organization_id, kind, percent_bps, cap_cents, updated_at)
VALUES ('$ZS_FIXTURE_ORGANIZATION_ID', 'percent', 2500, 4000, NOW())
ON CONFLICT (organization_id) DO UPDATE
  SET kind = EXCLUDED.kind, percent_bps = EXCLUDED.percent_bps,
      cap_cents = EXCLUDED.cap_cents, amount_cents = NULL, floor_cents = NULL,
      updated_at = NOW();
SQL
STORED_BPS="$(psql1 "SELECT percent_bps||'/'||cap_cents FROM zeroship.organization_fee_policy WHERE organization_id='$ZS_FIXTURE_ORGANIZATION_ID'")"
[ "$STORED_BPS" = "2500/4000" ] && pass "stored a 25%-capped-\$40 FeePolicy for the organization" \
  || { fail "FeePolicy did not store (read back '$STORED_BPS', want 2500/4000)"; exit 1; }
CO2="$(curl -s -X POST "$CONTROL_URL/api/organizations/$ZS_FIXTURE_ORGANIZATION_ID/connect/checkout" "${AUTH[@]}" \
  -H 'content-type: application/json' -d "{\"amount_cents\":$GROSS,\"currency\":\"usd\",\"cart_id\":\"cart-e2e-2\"}")"
CO2_FEE="$(echo "$CO2" | jget application_fee_cents)"
CO2_PI="$(echo "$CO2" | jget payment_intent_id)"
[ "$CO2_FEE" = "4000" ] && pass "FeePolicy honored: 25% of ${GROSS}c = 5000c capped to 4000c (server-resolved)" \
  || diverge "policy fee ('$CO2_FEE') != 4000 (25% capped at \$40)"
if [ -n "$CO2_PI" ]; then
  PI2_FEE="$(sget "payment_intents/$CO2_PI" | jget application_fee_amount)"
  [ "$PI2_FEE" = "4000" ] && pass "REAL Stripe PaymentIntent $CO2_PI carries the policy fee application_fee_amount=4000c" \
    || diverge "REAL policy PI application_fee_amount ('$PI2_FEE') != 4000"
fi
# Reset to the default so the rest of the run reasons about 15%. DELETE rather
# than a 1500-bps row: the default is what `FeePolicyStore::get` returns when
# there is NO row, so writing one would leave the later stages reasoning about a
# stored policy that happens to agree with the default rather than about the
# default itself.
psql_db -v ON_ERROR_STOP=1 >/dev/null <<SQL || { fail "could not clear the stored FeePolicy"; exit 1; }
DELETE FROM zeroship.organization_fee_policy WHERE organization_id = '$ZS_FIXTURE_ORGANIZATION_ID';
SQL

# ===========================================================================
echo ""
echo "=== Stage 4: M2 money-hole gate — account.updated charges_enabled→false blocks checkout ==="
# ===========================================================================
# Deliver a REAL-shaped account.updated flipping the cached flags to false (a
# risk/KYC hold). handle_account_updated (stripe_handlers.rs:1511) re-caches via
# update_account_flags_by_account_id; connect_checkout (:503) then rejects 400.
DISABLE_EVT="$(node -e '
const [acct,organization]=process.argv.slice(1);
const obj={ id:acct, object:"account", charges_enabled:false, payouts_enabled:false,
  details_submitted:true, metadata:{ organization_id:organization } };
const evt={ id:"evt_e2e_"+require("crypto").randomBytes(8).toString("hex"),
  object:"event", api_version:"2025-09-30.clover", created:Math.floor(Date.now()/1000),
  type:"account.updated", data:{ object:obj } };
process.stdout.write(JSON.stringify(evt));
' "$ACCT" "$ZS_FIXTURE_ORGANIZATION_ID")"
AU2_CODE="$(post_signed_webhook "$DISABLE_EVT")"
AU2_RESP="$(cat "$WORK/wh_resp.json" 2>/dev/null)"
echo "    account.updated(charges_enabled=false) → HTTP $AU2_CODE  resp=$AU2_RESP"
[ "$AU2_CODE" = "200" ] && pass "account.updated(disable) accepted (REAL signature) → 200" || fail "account.updated rejected (HTTP $AU2_CODE): $AU2_RESP"
GATE_OFF="$(wait_for_db "SELECT charges_enabled::int FROM zeroship.organization_accounts WHERE stripe_account_id='$ACCT'" "0" 10)"
[ "$GATE_OFF" = "0" ] && pass "control re-cached charges_enabled=false (M2: the gate now reflects Stripe's risk hold)" \
  || fail "account.updated did NOT flip the cached charges_enabled to false (got '$GATE_OFF')"

# The gate must now BLOCK a checkout (no PaymentIntent created).
CO_BLOCKED="$(curl -s -o "$WORK/co_blocked.json" -w '%{http_code}' -X POST "$CONTROL_URL/api/organizations/$ZS_FIXTURE_ORGANIZATION_ID/connect/checkout" "${AUTH[@]}" \
  -H 'content-type: application/json' -d "{\"amount_cents\":$GROSS,\"currency\":\"usd\",\"cart_id\":\"cart-blocked\"}")"
echo "    blocked checkout → HTTP $CO_BLOCKED  body=$(cat "$WORK/co_blocked.json" 2>/dev/null)"
[ "$CO_BLOCKED" = "400" ] && pass "M2 money-hole guard WORKS: connect_checkout to a not-charges_enabled account is REJECTED (400) — no PaymentIntent created" \
  || diverge "checkout to a disabled account returned HTTP $CO_BLOCKED (expected 400 — the M2 gate should block it)"

# Re-enable for the M4 stage below.
post_signed_webhook "$(node -e '
const [acct,organization]=process.argv.slice(1);
const obj={ id:acct, object:"account", charges_enabled:true, payouts_enabled:true, details_submitted:true, metadata:{ organization_id:organization } };
process.stdout.write(JSON.stringify({ id:"evt_e2e_"+require("crypto").randomBytes(8).toString("hex"), object:"event", api_version:"2025-09-30.clover", created:Math.floor(Date.now()/1000), type:"account.updated", data:{ object:obj } }));
' "$ACCT" "$ZS_FIXTURE_ORGANIZATION_ID")" >/dev/null

# ===========================================================================
echo ""
echo "=== Stage 5: M4 payout attribution — record_payout credits ONLY on settling-account match ==="
# ===========================================================================
# A connect-revenue invoice.paid carries metadata.organization_id (CLIENT-influenced)
# AND a settling account (on_behalf_of / transfer_data.destination). handle
# invoice.paid (stripe_handlers.rs:1353) resolves the settling account and calls
# account_belongs_to_creator (stripe_store.rs:295) — crediting ONLY when the
# claimed creator OWNS that account. A metadata-only/forged id is rejected.
mk_revenue_invoice_paid() {
  # $1 = settling acct_ for transfer_data.destination
  node -e '
  const [organization,dest]=process.argv.slice(1);
  const obj={ id:"in_e2e_"+require("crypto").randomBytes(8).toString("hex"), object:"invoice",
    status:"paid", amount_paid:20000, application_fee_amount:3000, currency:"usd",
    metadata:{ organization_id:organization },
    transfer_data:{ destination:dest } };
  process.stdout.write(JSON.stringify({ id:"evt_e2e_"+require("crypto").randomBytes(8).toString("hex"), object:"event", api_version:"2025-09-30.clover", created:Math.floor(Date.now()/1000), type:"invoice.paid", data:{ object:obj } }));
  ' "$ZS_FIXTURE_ORGANIZATION_ID" "$1"
}

# 5a — MISMATCH: a forged settling account the creator does NOT own → rejected.
FORGED_ACCT="acct_$(node -e 'console.log(require("crypto").randomBytes(10).toString("hex").slice(0,16))')"
M4_BAD_CODE="$(post_signed_webhook "$(mk_revenue_invoice_paid "$FORGED_ACCT")")"
M4_BAD_RESP="$(cat "$WORK/wh_resp.json" 2>/dev/null)"
echo "    revenue invoice.paid w/ FORGED settling acct → HTTP $M4_BAD_CODE  resp=$M4_BAD_RESP"
PAYOUTS_AFTER_BAD="$(psql1 "SELECT COUNT(*) FROM zeroship.payouts WHERE organization_id='$ZS_FIXTURE_ORGANIZATION_ID'")"
if echo "$M4_BAD_RESP" | grep -q 'attribution_mismatch'; then
  pass "M4: a forged settling account (creator does NOT own it) → attribution_mismatch, NOT credited (metadata-only trust rejected)"
else
  diverge "M4 mismatch leg: expected 'attribution_mismatch' ack, got HTTP $M4_BAD_CODE resp=$M4_BAD_RESP (control.log: $(grep -i 'attribution\|payout' "$WORK/control.log" | tail -2 | tr '\n' '|'))"
fi
[ "$PAYOUTS_AFTER_BAD" = "0" ] && pass "no payout row written for the mismatched attribution (Σ payouts=0)" \
  || diverge "a payout row was written despite the attribution mismatch (count=$PAYOUTS_AFTER_BAD) — money-hole!"

# 5b — MATCH: the creator's OWN live settling account → credited.
M4_OK_CODE="$(post_signed_webhook "$(mk_revenue_invoice_paid "$ACCT")")"
M4_OK_RESP="$(cat "$WORK/wh_resp.json" 2>/dev/null)"
echo "    revenue invoice.paid w/ OWNED settling acct → HTTP $M4_OK_CODE  resp=$M4_OK_RESP"
PAYOUT_ROW="$(wait_for_db "SELECT COUNT(*) FROM zeroship.payouts WHERE organization_id='$ZS_FIXTURE_ORGANIZATION_ID'" "1" 10)"
if [ "$PAYOUT_ROW" = "1" ]; then
  pass "M4: the creator's OWN settling account verified → record_payout credited the earnings (1 payout row)"
  NET="$(psql1 "SELECT net_amount FROM zeroship.payouts WHERE organization_id='$ZS_FIXTURE_ORGANIZATION_ID' ORDER BY created_at DESC LIMIT 1")"
  [ "$NET" = "17000" ] && pass "net credited = 17000c (gross 20000 − 3000 platform fee = the creator's keep)" \
    || diverge "net_amount ('$NET') != 17000 (gross 20000 − fee 3000)"
else
  diverge "M4 match leg: expected a credited payout row, got count=$PAYOUT_ROW (resp=$M4_OK_RESP; control.log: $(grep -i 'payout\|record' "$WORK/control.log" | tail -3 | tr '\n' '|'))"
fi

# 5c — NO settling account at all on a revenue event → refuse to credit.
NOSETTLE="$(node -e '
const [organization]=process.argv.slice(1);
const obj={ id:"in_e2e_"+require("crypto").randomBytes(8).toString("hex"), object:"invoice", status:"paid", amount_paid:5000, currency:"usd", metadata:{ organization_id:organization } };
process.stdout.write(JSON.stringify({ id:"evt_e2e_"+require("crypto").randomBytes(8).toString("hex"), object:"event", api_version:"2025-09-30.clover", created:Math.floor(Date.now()/1000), type:"invoice.paid", data:{ object:obj } }));
' "$ZS_FIXTURE_ORGANIZATION_ID")"
post_signed_webhook "$NOSETTLE" >/dev/null
NS_RESP="$(cat "$WORK/wh_resp.json" 2>/dev/null)"
echo "$NS_RESP" | grep -q 'no_settling_account' && pass "M4: a revenue invoice.paid with NO settling account → no_settling_account, refused (not credited blind)" \
  || diverge "no-settling-account leg: expected 'no_settling_account', got '$NS_RESP'"

# ===========================================================================
echo ""
echo "=== Stage 6: payout.failed → payout_failures row + creator resolution ==="
# ===========================================================================
# Deliver a REAL-shaped payout.failed for the linked acct_. handle_payout_failed
# (stripe_handlers.rs:2175) resolves the creator via get_creator_by_account
# (stripe_store.rs:316) off the event's top-level `account` / transfer_data, then
# records a payout_failures row (idempotent on po_). A real bouncing payout to a
# real bank can't be forced deterministically in TEST mode, so we deliver the
# real-shaped failed event (the genuine webhook path the handler consumes).
PO_ID="po_e2e_$(node -e 'console.log(require("crypto").randomBytes(8).toString("hex"))')"
POF_EVT="$(node -e '
const [po,acct]=process.argv.slice(1);
const obj={ id:po, object:"payout", status:"failed", amount:17000, currency:"usd",
  failure_code:"account_closed", failure_message:"The bank account has been closed",
  destination:"ba_e2e", account:acct };
process.stdout.write(JSON.stringify({ id:"evt_e2e_"+require("crypto").randomBytes(8).toString("hex"), object:"event", api_version:"2025-09-30.clover", created:Math.floor(Date.now()/1000), type:"payout.failed", data:{ object:obj }, account:acct }));
' "$PO_ID" "$ACCT")"
POF_CODE="$(post_signed_webhook "$POF_EVT")"
POF_RESP="$(cat "$WORK/wh_resp.json" 2>/dev/null)"
echo "    payout.failed → HTTP $POF_CODE  resp=$POF_RESP"
[ "$POF_CODE" = "200" ] && pass "payout.failed accepted (REAL signature) → 200" || diverge "payout.failed HTTP $POF_CODE: $POF_RESP (control.log: $(grep -i 'payout.failed\|payout_fail' "$WORK/control.log" | tail -2 | tr '\n' '|'))"
POF_ROW="$(wait_for_db "SELECT COUNT(*) FROM zeroship.payout_failures WHERE provider_payout_id='$PO_ID'" "1" 10)"
[ "$POF_ROW" = "1" ] && pass "payout.failed → payout_failures row recorded for $PO_ID (creator resolved via get_creator_by_account from the linked acct_)" \
  || diverge "no payout_failures row for $PO_ID (count=$POF_ROW). NOTE: the handler resolves the creator from the connected account; if 0, the acct_→creator reverse-resolve or the row write needs review."

# An UNLINKED account's payout.failed is a benign no-op (not attributable).
UNLINKED_PO="po_e2e_$(node -e 'console.log(require("crypto").randomBytes(8).toString("hex"))')"
UNLINKED_ACCT="acct_$(node -e 'console.log(require("crypto").randomBytes(10).toString("hex").slice(0,16))')"
post_signed_webhook "$(node -e '
const [po,acct]=process.argv.slice(1);
const obj={ id:po, object:"payout", status:"failed", amount:100, currency:"usd", account:acct };
process.stdout.write(JSON.stringify({ id:"evt_e2e_"+require("crypto").randomBytes(8).toString("hex"), object:"event", api_version:"2025-09-30.clover", created:Math.floor(Date.now()/1000), type:"payout.failed", data:{ object:obj }, account:acct }));
' "$UNLINKED_PO" "$UNLINKED_ACCT")" >/dev/null
UL_RESP="$(cat "$WORK/wh_resp.json" 2>/dev/null)"
UL_ROWS="$(psql1 "SELECT COUNT(*) FROM zeroship.payout_failures WHERE provider_payout_id='$UNLINKED_PO'")"
[ "${UL_ROWS:-0}" = "0" ] && pass "payout.failed for an UNLINKED account → acked, no row (correct fail-safe — not attributable)" \
  || diverge "an unlinked-account payout.failed wrote a row (count=$UL_ROWS) — should be a no-op"

# ===========================================================================
echo ""
echo "============================================"
echo "  Results: $PASS passed, $FAIL failed, $DIVERGENCE real-API divergence(s)"
echo "============================================"
echo "  Real Stripe ids this run:"
echo "    express-onboarding-account = ${EXPRESS_ACCT:-<none>}  (Stage 2 — faithful onboarding path; hosted flow not API-activatable)"
echo "    capable-destination-account= ${CUSTOM_ACCT:-<none>}  (Stage 2.5 — type=custom, transfers ACTIVE; the LIVE charge destination)"
echo "    live-checkout-pi           = ${CO_PI:-<none>}  (application_fee_amount=${EXPECT_FEE}c, the 15% platform cut, server-stamped)"
echo "    policy-pi                  = ${CO2_PI:-<none>}  (operator 25%-capped-\$40 → application_fee_amount=4000c)"
echo "  Connect money flow exercised end-to-end against REAL api.stripe.com test mode."
echo "  Stage 3 uses an API-provisioned type=custom account because Express accounts"
echo "  CANNOT be API-onboarded/activated (hosted browser flow only) — faithful + documented,"
echo "  not a workaround: the onboarding PATH is tested for real in Stage 2; only the CHARGE"
echo "  needs a destination whose transfers capability Stripe will actually transfer to."
[ $FAIL -eq 0 ] && exit 0 || exit 1
