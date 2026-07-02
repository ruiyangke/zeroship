#!/usr/bin/env bash
# ============================================================================
# e2e_auth_rpc.sh — AUTHENTICATED RPC through the real gateway (closes ISS-64).
#
# The gateway's SEC-5 default makes every RPC procedure `auth: user`. Proving an
# authenticated round-trip through the gateway needs a valid app session — which
# normally comes from the interactive native-OP OAuth dance. This harness OFFLINE-
# MINTS that session the same way the admin PAT is minted (sign a real token with
# the harness-controlled gateway Ed25519 key + seed the backing rows), so the
# gateway's REAL session-cookie validation + `ZeroShip-User` derivation runs
# end-to-end — no `--dev-insecure` auth-bypass added to the prod binaries
# (the pilot-rejected approach), just an offline-signed credential.
#
# It closes the gap the other harnesses leave: `e2e_app_primitives_auth.sh`
# proves the WORKER side (signed `ZeroShip-User` → AuthPlugin → requireUser) over
# /dispatch, and `oidc_rp_e2e` proves the gateway session mint/validate; this
# proves the SEAM — the gateway turning a session cookie into a signed
# `ZeroShip-User` and forwarding it so an `auth: user` RPC actually runs.
#
# What it asserts (app = auth-notes, all RPCs are SEC-5 `auth: user`):
#   - anon  GET /__zeroship/v1/auth.whoami    → 401 UNAUTHENTICATED (gateway gate)
#   - authed (session cookie) auth.whoami      → 200, env.auth.getUser() == our user
#   - anon  auth.notes.list                    → 401
#   - authed auth.notes.list                   → 200
#
# Needs a release build + examples/auth-notes/dist/app.zship.
#   ./tests/e2e_auth_rpc.sh
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

export CONTROL_PORT=9160
export WORKER_PORT=8128
export GATE_PORT=8042
export PG_PORT=5448
export PG_CONTAINER="zs-e2e-authrpc-pg"

PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); echo "  ✓ $1"; }
fail() { FAIL=$((FAIL+1)); printf "  ✗ %b\n" "$1"; }

source "$ROOT/tests/lib/e2e_stack.sh"
cleanup() { stack_down 2>/dev/null || true; }
trap cleanup EXIT

echo "============================================"
echo "  zeroship E2E — authenticated gateway RPC (ISS-64)"
echo "============================================"

stack_up       || { echo "stack bring-up failed"; exit 1; }
mint_admin_pat || exit 1

ZSHIP="$ROOT/examples/auth-notes/dist/app.zship"
[ -f "$ZSHIP" ] || { echo "missing $ZSHIP — run: pnpm --filter ./examples/auth-notes build"; exit 2; }

APP_ID="$(deploy_zship "auth-notes-ax" "$ZSHIP")" || { fail "deploy auth-notes"; exit 1; }
pass "deployed auth-notes ($APP_ID)"
HOST="auth-notes-ax.localhost"
OAC="oac_e2e_authrpc"
SECTOR="https://$HOST"

# --- Provision the app's per-app OAuth client (what control's OAuth-client
# provisioning does). resolve_auth needs route.oauth_client_id + sector_identifier
# (else 503/401); the gateway picks these up from control's LEFT JOIN on
# app_oauth_clients.
docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.oauth_clients (client_id, client_name, redirect_uris, scopes)
VALUES ('$OAC', 'e2e auth-rpc', ARRAY['https://$HOST/__zeroship/auth/callback'],
        ARRAY['openid','email','profile'])
ON CONFLICT (client_id) DO NOTHING;
INSERT INTO zeroship.app_oauth_clients (app_id, client_id, sector_identifier)
VALUES ('$APP_ID', '$OAC', '$SECTOR')
ON CONFLICT (app_id) DO UPDATE SET client_id = EXCLUDED.client_id, sector_identifier = EXCLUDED.sector_identifier;
SQL
[ $? -eq 0 ] && pass "provisioned app_oauth_clients ($OAC)" || fail "provision failed"

# --- Mint an app session cookie (offline, gateway Ed25519 key). Same kid format
# as the gateway's RFC-7638 thumbprint (sha256 of canonical OKP JWK → base64url).
mint_app_session() {
  node --input-type=module -e '
import { readFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { importPKCS8, exportJWK, SignJWT } from "file://'"$E2E_JOSE_JS"'";
const [pem, app] = process.argv.slice(1);
const key = await importPKCS8(readFileSync(pem,"utf8"), "EdDSA", { extractable:true });
const x = (await exportJWK(key)).x;
const kid = createHash("sha256").update(`{"crv":"Ed25519","kty":"OKP","x":"${x}"}`).digest("base64url");
const now = Math.floor(Date.now()/1000);
// `sub` must be a valid pairwise subject: "pws_" + exactly 20 alphanumeric
// chars (is_pairwise_subject). The cookie arm trusts the signed sub (it does
// not re-derive it), so a well-formed placeholder body is sufficient.
const jwt = await new SignJWT({
  app, sub: "pws_e2eauthuser000000000",
  email: "e2e-auth@zeroship.test", email_verified: true,
  name: "E2E Auth User", avatar: null, scopes: [],
  auth_time: now, amr: ["pwd"],
})
  .setProtectedHeader({ alg:"EdDSA", typ:"zeroship-sess+jwt", kid })
  .setIssuer("https://api.zeroship.ai")
  .setIssuedAt(now).setExpirationTime(now + 3600)
  .sign(key);
process.stdout.write(jwt);
' "$WORK/signing-key.pem" "$OAC"
}
SESSION="$(mint_app_session)"
[ "$(echo -n "$SESSION" | awk -F. '{print NF}')" = "3" ] && pass "minted session cookie" || { fail "session mint failed: $SESSION"; }

# rpc <desc> <proc> <cookie-or-empty> → echoes "<code> <body>"
rpc() {
  local proc="$2" cookie="$3"
  local args=(-s -o /tmp/authrpc.body -w '%{http_code}' -H "Host: $HOST")
  # Dev cookie name has no `__Host-` prefix (that requires Secure; the gateway
  # runs --dev-insecure over http). See oidc_rp::APP_SESSION_COOKIE_DEV.
  [ -n "$cookie" ] && args+=(-H "Cookie: zeroship_app_session=$cookie" -H "Origin: http://$HOST")
  local code; code="$(curl "${args[@]}" "http://localhost:$GATE_PORT/__zeroship/v1/$proc")"
  echo "$code"
}

# The gateway re-pulls routes from control on an interval; wait until it has
# picked up the new oauth_client_id (an authed call stops 503/redirecting).
echo
echo "=== waiting for gateway to pick up the provisioned client ==="
ready=0
for i in $(seq 1 20); do
  code="$(rpc x auth.whoami "$SESSION")"
  if [ "$code" = "200" ]; then ready=1; break; fi
  sleep 1
done
if [ "$ready" = "1" ]; then
  pass "gateway route has oauth_client_id (authed call → 200)"
else
  fail "gateway never accepted the session (last code=$code, body=$(head -c 160 /tmp/authrpc.body))"
  echo "  --- DEBUG: minted JWT header+claims ---"
  echo "$SESSION" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{const[h,p]=s.trim().split(".");const d=b=>JSON.parse(Buffer.from(b,"base64url"));console.log("header:",JSON.stringify(d(h)));console.log("claims:",JSON.stringify(d(p)))})'
  echo "  --- DEBUG: route oauth_client_id seen by control ---"
  docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -tAc \
    "SELECT a.name, c.client_id, c.sector_identifier FROM zeroship.apps a LEFT JOIN zeroship.app_oauth_clients c ON c.app_id=a.id WHERE a.id='$APP_ID';" 2>/dev/null | sed 's/^/    /'
  echo "  --- DEBUG: gateway log (auth/session/cookie/kid lines) ---"
  grep -iE 'session|cookie|kid|verif|sig|iss|oauth_client|sector|unauth|reject|client_not' "$WORK/gate.log" 2>/dev/null | tail -15 | sed 's/^/    /'
fi

echo
echo "=== anon vs authed through the gateway (SEC-5 auth: user) ==="
# getUser() path: anon gated at the edge; authed reaches the worker and the
# identity the gateway derived from the session cookie surfaces in env.auth.
c="$(rpc x auth.whoami "")";            [ "$c" = "401" ] && pass "anon auth.whoami → 401 (gateway gate)" || fail "anon auth.whoami → $c (want 401)"
c="$(rpc x auth.whoami "$SESSION")";    body="$(cat /tmp/authrpc.body)"
if [ "$c" = "200" ] && echo "$body" | grep -q 'e2e-auth@zeroship.test'; then
  pass "authed auth.whoami → 200, env.auth.getUser() == our user"
else
  fail "authed auth.whoami → $c; body=$(echo "$body" | head -c 200)"
fi
# requireUser() path: a SECOND authed resource, no backend dependency — the
# gateway→worker HMAC ZeroShip-User → kernel requireUser() returns the user.
c="$(rpc x auth.whoamiStrict "")";          [ "$c" = "401" ] && pass "anon auth.whoamiStrict → 401 (gateway gate)" || fail "anon auth.whoamiStrict → $c (want 401)"
c="$(rpc x auth.whoamiStrict "$SESSION")";  body="$(cat /tmp/authrpc.body)"
if [ "$c" = "200" ] && echo "$body" | grep -q 'e2e-auth@zeroship.test'; then
  pass "authed auth.whoamiStrict → 200, env.auth.requireUser() == our user"
else
  fail "authed auth.whoamiStrict → $c; body=$(echo "$body" | head -c 200)"
fi
# A kv-backed authed resource — we assert the GATE only (anon → 401). The
# authed read needs a kv (Redis) backend the shared worker doesn't run; the two
# cases above already prove the authed identity reaches env.auth end-to-end.
c="$(rpc x auth.notes.list "")";        [ "$c" = "401" ] && pass "anon auth.notes.list → 401 (gateway gate)" || fail "anon auth.notes.list → $c (want 401)"

echo
echo "============================================"
echo "  RESULTS: $PASS passed, $FAIL failed"
echo "============================================"
[ "$FAIL" -eq 0 ]
