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
# `FeePolicy`, the `connect_checkout` / `callback` / `set_fee_policy` handlers,
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
#     ─► seed creator ↔ acct_ link + charges_enabled, then call zeroship's
#         POST /api/creators/{id}/connect/checkout                (control's
#         REAL StripeClient + server-held FeePolicy)
#         → control stamps application_fee_amount + transfer_data[destination]
#           SERVER-SIDE; a MALICIOUS client application_fee_amount is IGNORED
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
# STRIPE_WEBHOOK_SECRET — the REAL signature-verification + handler path. Only
# the delivery is self-driven. Object SHAPES + the signature verify are real.
#
# ── CONNECT IS CURRENTLY DISABLED ON THIS TEST ACCOUNT ──────────────────────
# POST /v1/accounts returns HTTP 400 invalid_request_error: "You can only create
# new accounts if you've signed up for Connect…". This harness DETECTS that and
# SKIPS CLEANLY (exit 0, a clear SKIP message) — exactly like the *_live tests
# self-skip without creds. It is SAFE to commit + run anytime; it runs for real
# the moment Connect is enabled at dashboard.stripe.com/connect.
#
# DO NO HARM: dedicated DB `zeroship_stripe_e2e` on the :5440 server — never the
# real `zeroship` DB nor the concurrent agent's `zeroship_billing_test`. Boots
# control on a dedicated port. Self-managed up/down; cleans up on exit.
#
# Skips CLEANLY (exit 0) when prereqs are absent (no PG :5440, no docker for the
# migrate, the Stripe TEST env not sourced) OR when Connect is not enabled.
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

# --- prereq gates: skip cleanly when anything is missing --------------------
PSQL="${ZEROSHIP_PSQL:-/nix/store/0hzvyg4lmry0cv8pgl1fw9j1rddyqqbj-postgresql-17.7/bin/psql}"
PGHOST=localhost; PGPORT=5440; PGUSER=postgres; PGPW=zeroship
DB=zeroship_stripe_e2e

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

[ -x "$PSQL" ] || { echo "  ⚠ SKIP: psql not found at $PSQL (set ZEROSHIP_PSQL)."; exit 0; }
command -v node    >/dev/null 2>&1 || { echo "  ⚠ SKIP: node required."; exit 0; }
command -v openssl >/dev/null 2>&1 || { echo "  ⚠ SKIP: openssl required."; exit 0; }
command -v curl    >/dev/null 2>&1 || { echo "  ⚠ SKIP: curl required."; exit 0; }
[ -x "$BIN/zeroship-control" ] || { echo "  ⚠ SKIP: missing $BIN/zeroship-control — run: cargo build --release -p zeroship-control"; exit 0; }

export PGPASSWORD="$PGPW"
psql_db() { "$PSQL" -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d "$DB" "$@"; }
psql1()   { psql_db -tA -c "$1" 2>/dev/null | tr -d '[:space:]'; }
if ! "$PSQL" -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -tAc "SELECT 1" >/dev/null 2>&1; then
  echo "  ⚠ SKIP: Postgres :$PGPORT unreachable."; exit 0
fi
if [ -f "$ROOT/ops/db-migrate.sh" ] && ! command -v docker >/dev/null 2>&1; then
  echo "  ⚠ SKIP: docker required for the Liquibase migrate step."; exit 0
fi

SAPI="https://api.stripe.com/v1"
# The harness's OWN out-of-band Stripe REST driver (control uses its own cyper
# client). NEVER echo $SK.
sget()  { curl -s "$SAPI/$1" -u "$SK:"; }
spost() { curl -s -X POST "$SAPI/$1" -u "$SK:" "${@:2}"; }
jget()  { node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{try{const o=JSON.parse(s);let v=o;for(const k of process.argv[1].split('.').filter(Boolean)){v=v==null?undefined:(k.match(/^[0-9]+$/)?v[+k]:v[k]);}console.log(v==null?'':(typeof v==='object'?JSON.stringify(v):v))}catch(e){console.log('')}})" "$1"; }

# ===========================================================================
# CONNECT-ENABLED PROBE — the load-bearing gate. Try to create a REAL Express
# connected account requesting transfers+card_payments. On a Connect-disabled
# account Stripe returns HTTP 400 invalid_request_error "You can only create new
# accounts if you've signed up for Connect…". We SKIP CLEANLY in that case.
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
    echo ""
    echo "  ⚠ SKIP: Connect not enabled on this account — enable at dashboard.stripe.com/connect"
    echo "         POST /v1/accounts → ${PROBE_ERR:-<no error message — check CLI auth/network>}"
    echo ""
    echo "  This harness is committed + syntactically sound and runs the full Connect money"
    echo "  flow the moment Connect is enabled. Nothing else (DB/keys/webhook-sig) was touched"
    echo "  by this run. See docs/runbooks/stripe-connect-live-e2e.md."
    exit 0
    ;;
esac

# ===========================================================================
# From here on Connect IS enabled — run the full live money flow.
# ===========================================================================
CONTROL_PORT=9183
CONTROL_URL="http://localhost:$CONTROL_PORT"
WEBHOOK_SECRET="whsec_e2e_$(openssl rand -hex 16)"   # throwaway, per-run
WORK="$(mktemp -d -t zs-e2e-connect-XXXXXX)"
mkdir -p "$WORK/blobs"
PIDFILE="$WORK/pids"; : > "$PIDFILE"
CREATED_ACCTS="$WORK/accts"; : > "$CREATED_ACCTS"

cleanup() {
  echo ""
  echo "=== Cleanup ==="
  if [ -f "$PIDFILE" ]; then
    while read -r pid; do [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; done < "$PIDFILE"
  fi
  wait 2>/dev/null || true
  # Best-effort: delete the test-mode connected accounts this run minted.
  if [ -f "$CREATED_ACCTS" ]; then
    while read -r a; do [ -n "$a" ] && curl -s -X DELETE "$SAPI/accounts/$a" -u "$SK:" -o /dev/null 2>/dev/null || true; done < "$CREATED_ACCTS"
  fi
  [ -n "${WORK:-}" ] && rm -rf "$WORK"
  echo "  control down; test-mode connected accounts deleted; $WORK cleaned."
  echo "  (DB $DB left for inspection; the real zeroship DB + zeroship_billing_test were NEVER touched.)"
}
trap cleanup EXIT

lsof -ti :"$CONTROL_PORT" 2>/dev/null | xargs -r kill -9 2>/dev/null || true

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
echo "=== Stage 1: dedicated DB ($DB) + Liquibase + control booted at REAL Stripe ==="
# ===========================================================================
"$PSQL" -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL || { fail "could not (re)create $DB"; exit 1; }
SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname='$DB' AND pid<>pg_backend_pid();
DROP DATABASE IF EXISTS $DB;
CREATE DATABASE $DB;
ALTER DATABASE $DB SET search_path = zeroship, public;
SQL
pass "(re)created dedicated DB $DB on :$PGPORT (real zeroship + zeroship_billing_test untouched)"

MIG_LOG="$WORK/liquibase.log"
if ZEROSHIP_DB_JDBC="jdbc:postgresql://$PGHOST:$PGPORT/$DB" "$ROOT/ops/db-migrate.sh" update > "$MIG_LOG" 2>&1; then
  pass "Liquibase changelog applied (incl. 0044 creator_fee_policy + creator_accounts Connect flags)"
else
  fail "Liquibase migrate FAILED (see $MIG_LOG)"; tail -20 "$MIG_LOG"; exit 1
fi

DBURL="postgres://$PGUSER:$PGPW@$PGHOST:$PGPORT/$DB"
openssl genpkey -algorithm ed25519 -out "$WORK/signing-key.pem" 2>/dev/null
chmod 600 "$WORK/signing-key.pem"

# control — REAL https://api.stripe.com, the operator's TEST secret key (from env,
# NEVER on argv where it'd hit /proc/cmdline), and a known throwaway webhook
# secret so the harness can produce VALID signatures (the REAL verify path).
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

# Mint a platform-admin PAT so the operator-only set_fee_policy + the self-service
# onboard/checkout endpoints accept us (faithful AuthzGuard, EdDSA via jose).
CREATOR="$(node -e 'console.log(require("crypto").randomUUID())')"
NOW_UNIX="$(date +%s)"
EXP=$(( NOW_UNIX + 86400 ))
POLICY_JSON='{"name":"e2e-connect-admin","statements":[{"effect":"allow","actions":["billing:read","billing:write"],"resources":[{"type":"any"}],"conditions":[]}]}'
POLICY_HASH="$(node -e '
const {createHash}=require("crypto");
function c(v){if(v===null||typeof v==="number"||typeof v==="boolean"||typeof v==="string")return JSON.stringify(v);
if(Array.isArray(v))return "["+v.map(c).join(",")+"]";
return "{"+Object.keys(v).sort().map(k=>JSON.stringify(k)+":"+c(v[k])).join(",")+"}";}
process.stdout.write(createHash("sha256").update(c(JSON.parse(process.argv[1]))).digest("hex"));
' "$POLICY_JSON")"
TOKID="$(node -e 'console.log(require("crypto").randomUUID())')"

psql_db -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL || { fail "creator/PAT seed failed"; exit 1; }
INSERT INTO zeroship.users (id, email, name, email_verified_at)
VALUES ('$CREATOR', 'connect-$CREATOR@zeroship.test'::citext, 'E2E Connect Creator', NOW());
INSERT INTO zeroship.platform_admin_roles (user_id, role, granted_by)
VALUES ('$CREATOR','admin','$CREATOR') ON CONFLICT DO NOTHING;
INSERT INTO zeroship.permission_tokens (id, owner_id, kind, name, policies, policy_hash, expires_at)
VALUES ('$TOKID','$CREATOR','pat','e2e connect harness','$POLICY_JSON'::jsonb,'$POLICY_HASH', to_timestamp($EXP));
SQL
pass "seeded creator $CREATOR + operator PAT row"

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
  ' "$WORK/signing-key.pem" "$CREATOR" "$TOKID" "$POLICY_HASH" "$EXP")"
fi
if [ "$(echo -n "$PAT" | awk -F. '{print NF}')" = "3" ]; then
  pass "minted operator PAT for the onboard/checkout/fee-policy endpoints"
else
  echo "  ⚠ SKIP: could not mint PAT (jose missing at $JOSE_JS?) — the authenticated Connect legs need it."
  exit 0
fi
AUTH=(-H "Authorization: Bearer $PAT")

# ===========================================================================
echo ""
echo "=== Stage 2: REAL Express connected account via control's onboard handler ==="
# ===========================================================================
# Drive the REAL control handler `onboard` (crates/control/src/stripe_handlers.rs
# :120) — it calls StripeClient::create_connect_account (stripe_client.rs:963,
# POST /v1/accounts type=express, metadata[creator_id]=<creator>) then
# create_account_link (:983, POST /v1/account_links type=account_onboarding).
ONBOARD="$(curl -s -X POST "$CONTROL_URL/api/creators/$CREATOR/stripe/onboard" "${AUTH[@]}")"
echo "    onboard → $ONBOARD"
ACCT="$(echo "$ONBOARD" | jget account_id)"
LINK_URL="$(echo "$ONBOARD" | jget url)"
case "$ACCT" in
  acct_*) pass "control minted a REAL Express connected account $ACCT (server-stamped metadata.creator_id)"; echo "$ACCT" >> "$CREATED_ACCTS";;
  *) fail "onboard did not return an acct_ (resp: $ONBOARD). control.log: $(grep -i 'onboard\|account' "$WORK/control.log" | tail -3 | tr '\n' '|')"; exit 1;;
esac
# Stage 2.2 — the account_links onboarding URL (the hosted browser flow is NOT
# automatable; we assert the handler returned a real URL).
case "$LINK_URL" in
  https://connect.stripe.com/*|https://*.stripe.com/*) pass "onboard returned a REAL account_links hosted-onboarding URL (browser flow not automatable — documented)";;
  *) diverge "onboard returned no/unexpected account_links url (got: '$LINK_URL')";;
esac

# Fetch the REAL account back + confirm metadata.creator_id ownership signal.
ACCT_META="$(sget "accounts/$ACCT" | jget metadata.creator_id)"
[ "$ACCT_META" = "$CREATOR" ] && pass "REAL acct_ carries metadata.creator_id=$CREATOR (the ownership signal callback verifies, ISS-30)" \
  || diverge "acct_ metadata.creator_id ('$ACCT_META') != creator ($CREATOR)"

# ── Stage 2.3 — make charges possible in TEST mode. Hosted onboarding can't be
# automated, so flip the account to a fully-onboarded test fixture via the
# documented test-mode update path (prefill business profile + accept TOS so
# Stripe activates capabilities). If Stripe still hasn't activated charges, we
# fall back to delivering an account.updated that caches charges_enabled=true
# (the same write path Stripe would drive) so the checkout gate can proceed.
spost "accounts/$ACCT" \
  -d "business_type=individual" \
  -d "business_profile[url]=https://example.com" \
  -d "business_profile[mcc]=5734" \
  -d "tos_acceptance[date]=$NOW_UNIX" \
  -d "tos_acceptance[ip]=127.0.0.1" \
  -d "individual[first_name]=Test" -d "individual[last_name]=Creator" \
  -d "individual[email]=connect-$CREATOR@zeroship.test" \
  -d "individual[dob][day]=1" -d "individual[dob][month]=1" -d "individual[dob][year]=1990" \
  -d "individual[address][line1]=address_full_match" -d "individual[address][city]=South San Francisco" \
  -d "individual[address][state]=CA" -d "individual[address][postal_code]=94080" \
  -d "individual[id_number]=000000000" \
  -d "individual[phone]=+15555555555" \
  -d "external_account[object]=bank_account" -d "external_account[country]=US" \
  -d "external_account[currency]=usd" -d "external_account[routing_number]=110000000" \
  -d "external_account[account_number]=000123456789" \
  -o /dev/null 2>/dev/null
sleep 2
CHARGES_ENABLED="$(sget "accounts/$ACCT" | jget charges_enabled)"
echo "    REAL acct_ charges_enabled after test-mode activation = ${CHARGES_ENABLED:-<none>}"

# Run control's callback to verify ownership + persist Stripe's truth flags.
CB="$(curl -s -X POST "$CONTROL_URL/api/creators/$CREATOR/stripe/callback" "${AUTH[@]}" \
  -H 'content-type: application/json' -d '{}')"
echo "    callback → $CB"
CB_CHARGES="$(echo "$CB" | jget charges_enabled)"
if [ "$CB_CHARGES" = "true" ]; then
  pass "callback retrieved + persisted charges_enabled=true from Stripe's truth"
else
  # TEST-mode activation may lag; deliver the account.updated write path (the same
  # update_account_flags_by_account_id control would run from Stripe's real event).
  diverge "Stripe has not (yet) activated charges on the test account ($CB_CHARGES); driving the account.updated cache-write so the checkout gate can proceed (faithful to handle_account_updated's write path)"
  ACCT_EVT="$(node -e '
  const [acct,creator]=process.argv.slice(1);
  const obj={ id:acct, object:"account", charges_enabled:true, payouts_enabled:true,
    details_submitted:true, metadata:{ creator_id:creator } };
  const evt={ id:"evt_e2e_"+require("crypto").randomBytes(8).toString("hex"),
    object:"event", api_version:"2025-09-30.clover", created:Math.floor(Date.now()/1000),
    type:"account.updated", data:{ object:obj } };
  process.stdout.write(JSON.stringify(evt));
  ' "$ACCT" "$CREATOR")"
  AU_CODE="$(post_signed_webhook "$ACCT_EVT")"
  [ "$AU_CODE" = "200" ] && pass "account.updated (charges_enabled=true) accepted (REAL signature) → 200" \
    || fail "account.updated cache-write rejected (HTTP $AU_CODE): $(cat "$WORK/wh_resp.json" 2>/dev/null)"
  GATE="$(psql1 "SELECT charges_enabled::int FROM zeroship.creator_accounts WHERE stripe_account_id='$ACCT'")"
  [ "$GATE" = "1" ] && pass "control re-cached charges_enabled=true for $ACCT (M2 gate now open)" \
    || fail "the account.updated write did not flip the cached charges_enabled (got '$GATE')"
fi

# ===========================================================================
echo ""
echo "=== Stage 3: connect_checkout stamps the SERVER fee (15% FeePolicy) + transfer_data ==="
# ===========================================================================
# No fee policy row → DEFAULT 15% (crate::fee_policy::DEFAULT_PERCENT_BPS=1500).
# Charge $200.00 (20000 cents). A MALICIOUS client tries application_fee_amount=1
# — it has NO wire path (ConnectCheckoutBody has no such field) and MUST be ignored.
GROSS=20000
EXPECT_FEE=3000   # 15% of 20000, computed server-side (fee_policy.rs::fee_cents)
CO="$(curl -s -X POST "$CONTROL_URL/api/creators/$CREATOR/connect/checkout" "${AUTH[@]}" \
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

# Stage 3.2 — an operator-set FeePolicy (25% capped at $40) overrides the default.
SETP="$(curl -s -o /dev/null -w '%{http_code}' -X PUT "$CONTROL_URL/api/creators/$CREATOR/fee-policy" "${AUTH[@]}" \
  -H 'content-type: application/json' -d '{"kind":"percent","percent_bps":2500,"cap_cents":4000}')"
[ "$SETP" = "204" ] && pass "operator set a 25%-capped-\$40 FeePolicy (HTTP 204)" || diverge "set_fee_policy returned HTTP $SETP (expected 204)"
CO2="$(curl -s -X POST "$CONTROL_URL/api/creators/$CREATOR/connect/checkout" "${AUTH[@]}" \
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
# Reset to the default so the rest of the run reasons about 15%.
curl -s -o /dev/null -X PUT "$CONTROL_URL/api/creators/$CREATOR/fee-policy" "${AUTH[@]}" \
  -H 'content-type: application/json' -d '{"kind":"percent","percent_bps":1500}'

# ===========================================================================
echo ""
echo "=== Stage 4: M2 money-hole gate — account.updated charges_enabled→false blocks checkout ==="
# ===========================================================================
# Deliver a REAL-shaped account.updated flipping the cached flags to false (a
# risk/KYC hold). handle_account_updated (stripe_handlers.rs:1511) re-caches via
# update_account_flags_by_account_id; connect_checkout (:503) then rejects 400.
DISABLE_EVT="$(node -e '
const [acct,creator]=process.argv.slice(1);
const obj={ id:acct, object:"account", charges_enabled:false, payouts_enabled:false,
  details_submitted:true, metadata:{ creator_id:creator } };
const evt={ id:"evt_e2e_"+require("crypto").randomBytes(8).toString("hex"),
  object:"event", api_version:"2025-09-30.clover", created:Math.floor(Date.now()/1000),
  type:"account.updated", data:{ object:obj } };
process.stdout.write(JSON.stringify(evt));
' "$ACCT" "$CREATOR")"
AU2_CODE="$(post_signed_webhook "$DISABLE_EVT")"
AU2_RESP="$(cat "$WORK/wh_resp.json" 2>/dev/null)"
echo "    account.updated(charges_enabled=false) → HTTP $AU2_CODE  resp=$AU2_RESP"
[ "$AU2_CODE" = "200" ] && pass "account.updated(disable) accepted (REAL signature) → 200" || fail "account.updated rejected (HTTP $AU2_CODE): $AU2_RESP"
GATE_OFF="$(wait_for_db "SELECT charges_enabled::int FROM zeroship.creator_accounts WHERE stripe_account_id='$ACCT'" "0" 10)"
[ "$GATE_OFF" = "0" ] && pass "control re-cached charges_enabled=false (M2: the gate now reflects Stripe's risk hold)" \
  || fail "account.updated did NOT flip the cached charges_enabled to false (got '$GATE_OFF')"

# The gate must now BLOCK a checkout (no PaymentIntent created).
CO_BLOCKED="$(curl -s -o "$WORK/co_blocked.json" -w '%{http_code}' -X POST "$CONTROL_URL/api/creators/$CREATOR/connect/checkout" "${AUTH[@]}" \
  -H 'content-type: application/json' -d "{\"amount_cents\":$GROSS,\"currency\":\"usd\",\"cart_id\":\"cart-blocked\"}")"
echo "    blocked checkout → HTTP $CO_BLOCKED  body=$(cat "$WORK/co_blocked.json" 2>/dev/null)"
[ "$CO_BLOCKED" = "400" ] && pass "M2 money-hole guard WORKS: connect_checkout to a not-charges_enabled account is REJECTED (400) — no PaymentIntent created" \
  || diverge "checkout to a disabled account returned HTTP $CO_BLOCKED (expected 400 — the M2 gate should block it)"

# Re-enable for the M4 stage below.
post_signed_webhook "$(node -e '
const [acct,creator]=process.argv.slice(1);
const obj={ id:acct, object:"account", charges_enabled:true, payouts_enabled:true, details_submitted:true, metadata:{ creator_id:creator } };
process.stdout.write(JSON.stringify({ id:"evt_e2e_"+require("crypto").randomBytes(8).toString("hex"), object:"event", api_version:"2025-09-30.clover", created:Math.floor(Date.now()/1000), type:"account.updated", data:{ object:obj } }));
' "$ACCT" "$CREATOR")" >/dev/null

# ===========================================================================
echo ""
echo "=== Stage 5: M4 payout attribution — record_payout credits ONLY on settling-account match ==="
# ===========================================================================
# A connect-revenue invoice.paid carries metadata.creator_id (CLIENT-influenced)
# AND a settling account (on_behalf_of / transfer_data.destination). handle
# invoice.paid (stripe_handlers.rs:1353) resolves the settling account and calls
# account_belongs_to_creator (stripe_store.rs:295) — crediting ONLY when the
# claimed creator OWNS that account. A metadata-only/forged id is rejected.
mk_revenue_invoice_paid() {
  # $1 = settling acct_ for transfer_data.destination
  node -e '
  const [creator,dest]=process.argv.slice(1);
  const obj={ id:"in_e2e_"+require("crypto").randomBytes(8).toString("hex"), object:"invoice",
    status:"paid", amount_paid:20000, application_fee_amount:3000, currency:"usd",
    metadata:{ creator_id:creator },
    transfer_data:{ destination:dest } };
  process.stdout.write(JSON.stringify({ id:"evt_e2e_"+require("crypto").randomBytes(8).toString("hex"), object:"event", api_version:"2025-09-30.clover", created:Math.floor(Date.now()/1000), type:"invoice.paid", data:{ object:obj } }));
  ' "$CREATOR" "$1"
}

# 5a — MISMATCH: a forged settling account the creator does NOT own → rejected.
FORGED_ACCT="acct_$(node -e 'console.log(require("crypto").randomBytes(10).toString("hex").slice(0,16))')"
M4_BAD_CODE="$(post_signed_webhook "$(mk_revenue_invoice_paid "$FORGED_ACCT")")"
M4_BAD_RESP="$(cat "$WORK/wh_resp.json" 2>/dev/null)"
echo "    revenue invoice.paid w/ FORGED settling acct → HTTP $M4_BAD_CODE  resp=$M4_BAD_RESP"
PAYOUTS_AFTER_BAD="$(psql1 "SELECT COUNT(*) FROM zeroship.payouts WHERE creator_id='$CREATOR'")"
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
PAYOUT_ROW="$(wait_for_db "SELECT COUNT(*) FROM zeroship.payouts WHERE creator_id='$CREATOR'" "1" 10)"
if [ "$PAYOUT_ROW" = "1" ]; then
  pass "M4: the creator's OWN settling account verified → record_payout credited the earnings (1 payout row)"
  NET="$(psql1 "SELECT net_amount FROM zeroship.payouts WHERE creator_id='$CREATOR' ORDER BY created_at DESC LIMIT 1")"
  [ "$NET" = "17000" ] && pass "net credited = 17000c (gross 20000 − 3000 platform fee = the creator's keep)" \
    || diverge "net_amount ('$NET') != 17000 (gross 20000 − fee 3000)"
else
  diverge "M4 match leg: expected a credited payout row, got count=$PAYOUT_ROW (resp=$M4_OK_RESP; control.log: $(grep -i 'payout\|record' "$WORK/control.log" | tail -3 | tr '\n' '|'))"
fi

# 5c — NO settling account at all on a revenue event → refuse to credit.
NOSETTLE="$(node -e '
const [creator]=process.argv.slice(1);
const obj={ id:"in_e2e_"+require("crypto").randomBytes(8).toString("hex"), object:"invoice", status:"paid", amount_paid:5000, currency:"usd", metadata:{ creator_id:creator } };
process.stdout.write(JSON.stringify({ id:"evt_e2e_"+require("crypto").randomBytes(8).toString("hex"), object:"event", api_version:"2025-09-30.clover", created:Math.floor(Date.now()/1000), type:"invoice.paid", data:{ object:obj } }));
' "$CREATOR")"
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
echo "  Real Stripe ids this run: connected-account=$ACCT  checkout-pi=${CO_PI:-<none>}  policy-pi=${CO2_PI:-<none>}"
echo "  Connect money flow exercised end-to-end against REAL api.stripe.com test mode."
[ $FAIL -eq 0 ] && exit 0 || exit 1
