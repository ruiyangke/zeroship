#!/usr/bin/env bash
# ============================================================================
# tests/e2e_device_login.sh
#
# `zeroship login` END TO END, THE WAY A HUMAN DOES IT, on the SHIPPED DEFAULT
# configuration: the platform's own OP, no Supabase variables set anywhere.
#
# What makes this harness worth its runtime is the ONE step that no other test
# in the tree performs: it opens the verification URI the CLI printed, in the
# browser's shoes, and submits the approval FORM.
#
#   real `zeroship login` process
#     -> control POST /api/device/auth        (user code + verification URI)
#     -> GET  {verification_uri}?user_code=..  with a real signed-in session
#     -> POST /device  csrf + user_code + confirm=authorize
#     -> the CLI's poll returns a token
#     -> `zeroship deploy` with that token
#     -> the gateway serves the deployed app
#
# `tests/supabase_deploy_e2e.sh` curls `/api/device/approve` directly, which is
# exactly the shortcut that let the browser leg stay broken in BOTH provider
# configurations: control wrote `provider = 'platform'` rows and the only page
# that could approve anything filtered `provider = 'op'`, so every code the CLI
# printed read back as "invalid or expired code". A harness that never loads
# the page cannot see that.
#
# Usage:  ./tests/e2e_device_login.sh
# Needs:  docker, curl, node, openssl, and a release build:
#           cargo build --release -p zeroship -p zeroship-control \
#             -p zeroship-gateway -p zeroship-worker -p zeroship-auth
#           cargo build --release -p zeroship-migrate-adapter \
#             --features platform-cli --bin zeroship-platform-migrate
#         plus a built example artifact:
#           pnpm --filter ./examples/auth-probe build
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release"

PASS=0; FAIL=0; KNOWN=0; SKIPPED=0
pass() { PASS=$((PASS + 1)); echo "  PASS $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL $1" >&2; }
step() { echo ""; echo "== $1"; }

# --- our OWN port band and container name -----------------------------------
# Other harnesses and other agents' stacks live on this machine. Nothing below
# reuses a port, a container, or a database it did not create.
export AUTH_PORT="${AUTH_PORT:-9481}"
export ZEROSHIP_CONTROL_PORT="${ZEROSHIP_CONTROL_PORT:-9480}"
export ZEROSHIP_WORKER_PORT="${ZEROSHIP_WORKER_PORT:-8481}"
export ZEROSHIP_GATEWAY_PORT="${ZEROSHIP_GATEWAY_PORT:-8382}"
export PG_PORT="${PG_PORT:-5481}"
export PG_CONTAINER="${PG_CONTAINER:-zs-devlogin-pg}"

AUTH_URL="http://localhost:$AUTH_PORT"
CONTROL_URL="http://localhost:$ZEROSHIP_CONTROL_PORT"
GATE_URL="http://localhost:$ZEROSHIP_GATEWAY_PORT"

# THE POINT OF THE WHOLE HARNESS: nothing Supabase-shaped is configured. If any
# of these leak in from the caller's environment the run is not testing the
# shipped default, so drop them rather than silently measuring something else.
unset ZEROSHIP_AUTH_PROVIDER ZEROSHIP_CONTROL_AUTH_PROVIDER AUTH_PROVIDER
unset SUPABASE_URL SUPABASE_ANON_KEY SUPABASE_SERVICE_ROLE_KEY SUPABASE_JWT_SECRET
unset ZEROSHIP_SUPABASE_URL ZEROSHIP_SUPABASE_ANON_KEY

# shellcheck source=tests/lib/e2e_stack.sh
source "$ROOT/tests/lib/e2e_stack.sh"

APP_SLUG="devlogin-$$"
ZSHIP="$ROOT/examples/auth-probe/dist/app.zship"

cleanup() {
  if [ -n "${PIDFILE:-}" ] && [ -f "$PIDFILE" ]; then
    while read -r pid; do kill "$pid" 2>/dev/null || true; done < "$PIDFILE"
  fi
  [ -n "${CLI_PID:-}" ] && kill "$CLI_PID" 2>/dev/null
  docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
  [ -n "${WORK:-}" ] && [ -d "$WORK" ] && rm -rf "$WORK"
  return 0
}
trap cleanup EXIT

# Read `value` out of `<input name="NAME" value="VALUE">`.
form_field() {
  node -e '
const fs=require("fs");
const html=fs.readFileSync(process.argv[1],"utf8");
const re=new RegExp("<input[^>]*name=\""+process.argv[2]+"\"[^>]*>","i");
const m=html.match(re);
if(!m){process.stdout.write("");process.exit(0)}
const v=m[0].match(/value="([^"]*)"/i);
process.stdout.write(v?v[1].replace(/&amp;/g,"&").replace(/&quot;/g,"\"").replace(/&#x27;/g,"'"'"'").replace(/&lt;/g,"<").replace(/&gt;/g,">"):"");
' "$1" "$2"
}

jget() { node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{try{const o=JSON.parse(s);process.stdout.write(String(o$1??'')+'\n')}catch(e){console.log('')}})"; }

jwt_claim() {
  node -e "
const p=process.argv[1].split('.')[1];
const c=JSON.parse(Buffer.from(p,'base64url').toString('utf8'));
process.stdout.write(String(c[process.argv[2]] ?? '')+'\n');
" "$1" "$2"
}

psql_q() { docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -tAc "$1" 2>/dev/null | tr -d '[:space:]'; }

# ---------------------------------------------------------------------------
step "Preflight"
stack_preflight || exit 2
[ -x "$BIN/zeroship-auth" ] || { fail "missing $BIN/zeroship-auth"; exit 2; }
if [ ! -f "$ZSHIP" ]; then
  e2e_skipped "no $ZSHIP; run: pnpm --filter ./examples/auth-probe build"
  e2e_verdict || exit 1
  exit 0
fi
pass "binaries and $ZSHIP present"

stack_workspace || { fail "workspace"; exit 1; }
stack_pg_up || { fail "postgres + migrations"; exit 1; }

# The CLI must not touch the operator's real credentials file.
export ZEROSHIP_CONFIG_HOME="$WORK/cli-config"
mkdir -p "$ZEROSHIP_CONFIG_HOME"

for p in $AUTH_PORT $ZEROSHIP_CONTROL_PORT $ZEROSHIP_WORKER_PORT $ZEROSHIP_GATEWAY_PORT; do
  lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
done

# ---------------------------------------------------------------------------
step "Boot the platform-only stack"
echo "  auth=$AUTH_URL control=$CONTROL_URL gateway=$GATE_URL pg=:$PG_PORT"
echo "  platform issuer: $ZEROSHIP_AUTH_PLATFORM_ISSUER"

"$BIN/zeroship-auth" \
  --addr "0.0.0.0:$AUTH_PORT" --db-url "$DBURL" --public-url "$AUTH_URL" \
  --control-url "$CONTROL_URL" \
  --stash-signing-key "$STASH_SIGNING_KEY" \
  --totp-enc-key "$AUTH_TOTP_ENC_KEY" \
  --auth-signing-key-file "$AUTH_SIGNING_KEY_FILE" \
  --auth-pairwise-salt-file "$AUTH_PAIRWISE_SALT_FILE" \
  --auth-broker-secret-file "$AUTH_BROKER_SECRET_FILE" \
  --refresh-hash-key-file "$REFRESH_HASH_KEY_FILE" \
  --refresh-idem-key-file "$REFRESH_IDEM_KEY_FILE" \
  --mailer stdout --relay-forward-mailer stdout \
  > "$WORK/auth.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 40); do curl -sf "$AUTH_URL/healthz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "$AUTH_URL/oauth2/.well-known/jwks.json" >/dev/null 2>&1 \
  && pass "auth (platform OP) healthy" \
  || { fail "auth never came up"; tail -30 "$WORK/auth.log"; exit 1; }

"$BIN/zeroship-control" --port "$ZEROSHIP_CONTROL_PORT" --db "$DBURL" \
  --blob-store "$WORK/blobs" --signing-key-file "$WORK/signing-key.pem" \
  --app-base-domain "localhost" --pairwise-salt "$PAIRWISE_SALT" \
  > "$WORK/control.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "$CONTROL_URL/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "$CONTROL_URL/health" >/dev/null 2>&1 \
  && pass "control healthy" || { fail "control unhealthy"; tail -30 "$WORK/control.log"; exit 1; }

"$BIN/zeroship-worker" --port "$ZEROSHIP_WORKER_PORT" --threads 2 \
  --control-url "$CONTROL_URL" --db "$DBURL" \
  --blob-store "$WORK/blobs" --poll-interval 2 > "$WORK/worker.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "http://localhost:$ZEROSHIP_WORKER_PORT/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$ZEROSHIP_WORKER_PORT/health" >/dev/null 2>&1 \
  && pass "worker healthy" || { fail "worker unhealthy"; tail -30 "$WORK/worker.log"; exit 1; }

"$BIN/zeroship-gate" --port "$ZEROSHIP_GATEWAY_PORT" --control-url "$CONTROL_URL" \
  --worker-urls "http://localhost:$ZEROSHIP_WORKER_PORT" --blob-store "$WORK/blobs" \
  --blob-cache-disk-root "$WORK/blob-cache" --db "$DBURL" --poll-interval 2 \
  --signing-key-file "$WORK/signing-key.pem" \
  --gateway-broker-secret-file "$WORK/gate-secret" \
  --auth-ui-url "$AUTH_URL" \
  --stash-signing-key "$STASH_SIGNING_KEY" --pairwise-salt "$PAIRWISE_SALT" \
  > "$WORK/gate.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "$GATE_URL/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "$GATE_URL/health" >/dev/null 2>&1 \
  && pass "gateway healthy" || { fail "gateway unhealthy"; tail -30 "$WORK/gate.log"; exit 1; }

# ---------------------------------------------------------------------------
step "Create the human, through the product's own signup"
# `/signup` requires exactly one `return_to` and it must parse as a real
# `/oauth2/authorize` target (crates/auth/src/ui/signup.rs). That continuation
# is scaffolding for reaching signup at all, not the subject of this test, so
# the client it names is inserted directly rather than brokered through a
# deploy we have not done yet.
SIGNUP_CLIENT="oac_devlogin_$$"
docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -q >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.oauth_clients (client_id, client_name, redirect_uris, scopes)
VALUES ('$SIGNUP_CLIENT', 'device login harness', ARRAY['http://localhost/cb'], ARRAY['openid'])
ON CONFLICT (client_id) DO NOTHING;
SQL
RETURN_TO="/oauth2/authorize?response_type=code&client_id=$SIGNUP_CLIENT&redirect_uri=http%3A%2F%2Flocalhost%2Fcb&scope=openid&state=s&nonce=n&code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM&code_challenge_method=S256"

EMAIL="creator-$$@zeroship.test"
PASSWORD="correct horse battery staple $$"
JAR="$WORK/session-jar.txt"
rm -f "$JAR"

curl -s -c "$JAR" -m 15 -o "$WORK/signup.html" -G "$AUTH_URL/signup" \
  --data-urlencode "return_to=$RETURN_TO" >/dev/null
SIGNUP_CSRF="$(form_field "$WORK/signup.html" csrf)"
[ -n "$SIGNUP_CSRF" ] || { fail "no csrf on the signup form"; exit 1; }
SIGNUP_CODE="$(curl -s -b "$JAR" -c "$JAR" -m 30 -o "$WORK/signup-post.html" -w '%{http_code}' \
  -X POST "$AUTH_URL/signup" \
  --data-urlencode "csrf=$SIGNUP_CSRF" --data-urlencode "return_to=$RETURN_TO" \
  --data-urlencode "name=Device Login Creator" --data-urlencode "email=$EMAIL" \
  --data-urlencode "password=$PASSWORD")"
USER_ID="$(psql_q "SELECT id FROM zeroship.users WHERE email = '$EMAIL'::citext")"
[ -n "$USER_ID" ] && pass "signup created $EMAIL ($USER_ID), HTTP $SIGNUP_CODE" \
  || { fail "signup did not create a user (HTTP $SIGNUP_CODE)"; exit 1; }

# B2 regression, checked BEFORE the login: the auth service cannot write
# `zeroship.principal_grants` (grants.ts gives that table to zeroship_control
# only), so a freshly signed-up platform creator has NO grants. If control does
# not provision them the deploy token mints with `scope: ""`.
GRANTS_AT_SIGNUP="$(psql_q "SELECT count(*) FROM zeroship.principal_grants WHERE principal_id = '$USER_ID'")"
[ "$GRANTS_AT_SIGNUP" = "0" ] \
  && pass "a fresh platform principal has 0 grants (so the mint must provision them)" \
  || fail "expected 0 grants at signup, got $GRANTS_AT_SIGNUP"

step "Log in, the way a browser does"
rm -f "$JAR"
curl -s -c "$JAR" -m 15 -o "$WORK/login.html" "$AUTH_URL/login" >/dev/null
LOGIN_CSRF="$(form_field "$WORK/login.html" csrf)"
[ -n "$LOGIN_CSRF" ] || { fail "no csrf on the login form"; exit 1; }
LOGIN_CODE="$(curl -s -b "$JAR" -c "$JAR" -m 30 -o "$WORK/login-post.html" -w '%{http_code}' \
  -X POST "$AUTH_URL/login" \
  --data-urlencode "csrf=$LOGIN_CSRF" --data-urlencode "email=$EMAIL" \
  --data-urlencode "password=$PASSWORD")"
grep -q '__Host-zsidp_session' "$JAR" \
  && pass "login set the OP session cookie (HTTP $LOGIN_CODE)" \
  || { fail "login left no session cookie (HTTP $LOGIN_CODE)"; head -40 "$WORK/login-post.html"; exit 1; }

# ---------------------------------------------------------------------------
step "Run the REAL zeroship login and read what it tells the human"
CLI_OUT="$WORK/cli-login.txt"
( "$BIN/zeroship" login --control="$CONTROL_URL" > "$CLI_OUT" 2>&1; echo "$?" > "$WORK/cli.rc" ) &
CLI_PID=$!

USER_CODE=""; VERIFY_URI=""
for _ in $(seq 1 40); do
  USER_CODE="$(grep -oE '[BCDFGHJKLMNPQRSTVWXZ]{4}-[BCDFGHJKLMNPQRSTVWXZ]{4}-[BCDFGHJKLMNPQRSTVWXZ]{4}' "$CLI_OUT" 2>/dev/null | head -1)"
  VERIFY_URI="$(grep -oE 'https?://[^ ]+/device[^ ]*' "$CLI_OUT" 2>/dev/null | head -1)"
  [ -n "$USER_CODE" ] && [ -n "$VERIFY_URI" ] && break
  sleep 0.5
done
echo "--- zeroship login said: ---"
sed 's/^/  | /' "$CLI_OUT"
echo "----------------------------"
[ -n "$USER_CODE" ] && [ -n "$VERIFY_URI" ] \
  && pass "CLI printed user_code=$USER_CODE and a verification URI" \
  || { fail "CLI printed no usable code/URI"; cat "$CLI_OUT"; exit 1; }

# The URI must name the host that actually serves the page. Before the fix this
# was `{scheme}://auth.{app_base_domain}/device` -- a guess -- and a human
# following it reached nothing at all.
case "$VERIFY_URI" in
  "$AUTH_URL/device"*) pass "verification URI points at the configured OP: $VERIFY_URI" ;;
  *) fail "verification URI $VERIFY_URI is not on $AUTH_URL" ;;
esac

# ---------------------------------------------------------------------------
step "Approve THROUGH THE PAGE, as a human would"
DEV_GET="$WORK/device-get.html"
DEV_STATUS="$(curl -s -b "$JAR" -c "$JAR" -m 15 -o "$DEV_GET" -w '%{http_code}' "$VERIFY_URI")"
echo "  GET $VERIFY_URI -> HTTP $DEV_STATUS"
[ "$DEV_STATUS" = "200" ] || { fail "device page returned HTTP $DEV_STATUS"; head -40 "$DEV_GET"; exit 1; }
if grep -q 'invalid or expired code' "$DEV_GET"; then
  fail "the device page does not recognise the code the CLI printed"
  head -40 "$DEV_GET"
  exit 1
fi
grep -q 'the zeroship CLI' "$DEV_GET" \
  && pass "the page renders the confirmation for the zeroship CLI" \
  || fail "confirmation page did not name the CLI: $(head -c 400 "$DEV_GET")"
for scope in 'apps:deploy' 'apps:read' 'apps:write'; do
  grep -q "$scope" "$DEV_GET" && pass "the page discloses $scope" || fail "the page hid $scope"
done

DEV_CSRF="$(form_field "$DEV_GET" csrf)"
DEV_FORM_CODE="$(form_field "$DEV_GET" user_code)"
[ -n "$DEV_CSRF" ] || { fail "no csrf on the device confirmation form"; exit 1; }
[ "$DEV_FORM_CODE" = "$USER_CODE" ] \
  && pass "the form carries the code back ($DEV_FORM_CODE)" \
  || fail "form user_code $DEV_FORM_CODE != $USER_CODE"

DEV_POST="$WORK/device-post.html"
POST_STATUS="$(curl -s -b "$JAR" -c "$JAR" -m 20 -o "$DEV_POST" -w '%{http_code}' \
  -X POST "$AUTH_URL/device" \
  --data-urlencode "csrf=$DEV_CSRF" \
  --data-urlencode "user_code=$USER_CODE" \
  --data-urlencode "confirm=authorize")"
echo "  POST $AUTH_URL/device (csrf, user_code=$USER_CODE, confirm=authorize) -> HTTP $POST_STATUS"
[ "$POST_STATUS" = "200" ] && grep -q 'Device approved' "$DEV_POST" \
  && pass "the browser approved the grant" \
  || { fail "approval POST returned HTTP $POST_STATUS"; head -40 "$DEV_POST"; exit 1; }

APPROVED_PRINCIPAL="$(psql_q "SELECT principal_id FROM zeroship.device_grants WHERE user_code = '$USER_CODE'")"
[ "$APPROVED_PRINCIPAL" = "$USER_ID" ] \
  && pass "the grant is bound to the signed-in user" \
  || echo "  (grant row already redeemed by the CLI poll: principal='$APPROVED_PRINCIPAL')"

# ---------------------------------------------------------------------------
step "The CLI's poll returns a usable token"
for _ in $(seq 1 60); do [ -f "$WORK/cli.rc" ] && break; sleep 1; done
CLI_RC="$(cat "$WORK/cli.rc" 2>/dev/null || echo "timeout")"
echo "--- zeroship login final output (rc=$CLI_RC): ---"
sed 's/^/  | /' "$CLI_OUT"
echo "------------------------------------------------"
[ "$CLI_RC" = "0" ] && pass "zeroship login exited 0" || { fail "zeroship login exited $CLI_RC"; exit 1; }
grep -q "Signed in as $USER_ID" "$CLI_OUT" \
  && pass "the CLI reports the signed-in principal" \
  || fail "CLI did not report principal $USER_ID"

CREDS="$ZEROSHIP_CONFIG_HOME/zeroship/credentials.json"
[ -f "$CREDS" ] || CREDS="$(find "$ZEROSHIP_CONFIG_HOME" -name '*.json' -type f | head -1)"
[ -f "$CREDS" ] || { fail "no credentials file under $ZEROSHIP_CONFIG_HOME"; exit 1; }
TOKEN="$(jget '.access_token' < "$CREDS")"
[ -n "$TOKEN" ] || { fail "credentials carry no access_token"; exit 1; }

TOKEN_SCOPE="$(jwt_claim "$TOKEN" scope)"
TOKEN_SUB="$(jwt_claim "$TOKEN" sub)"
TOKEN_EXP="$(jwt_claim "$TOKEN" exp)"
TOKEN_IAT="$(jwt_claim "$TOKEN" iat)"
TOKEN_TTL=$((TOKEN_EXP - TOKEN_IAT))
echo "  token: sub=$TOKEN_SUB scope='$TOKEN_SCOPE' ttl=${TOKEN_TTL}s"
# B2: an empty scope here is the bug that made approval succeed and deploy 403.
[ "$TOKEN_SCOPE" = "apps:deploy apps:read apps:write" ] \
  && pass "the deploy token carries the creator scopes" \
  || fail "deploy token scope was '$TOKEN_SCOPE'"
[ "$TOKEN_SUB" = "$USER_ID" ] && pass "the token's subject is the approving user" \
  || fail "token sub $TOKEN_SUB != $USER_ID"
# B3: 15 minutes is the OP default and is not a CLI session.
[ "$TOKEN_TTL" -ge 3600 ] \
  && pass "the deploy token outlives a single command (${TOKEN_TTL}s)" \
  || fail "deploy token TTL is only ${TOKEN_TTL}s"

GRANTS_AFTER="$(psql_q "SELECT string_agg(grant_name, ',' ORDER BY grant_name) FROM zeroship.principal_grants WHERE principal_id = '$USER_ID'")"
[ "$GRANTS_AFTER" = "apps:deploy,apps:read,apps:write" ] \
  && pass "control provisioned the creator grants ($GRANTS_AFTER)" \
  || fail "unexpected grants after login: '$GRANTS_AFTER'"

# ---------------------------------------------------------------------------
step "Deploy with that token, and serve it"
APP_ID="$(curl -sS -X POST "$CONTROL_URL/api/apps" -H "Authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' -d "{\"name\":\"$APP_SLUG\"}" | jget '.id')"
[ -n "$APP_ID" ] && pass "created app $APP_SLUG ($APP_ID) with the login token" \
  || { fail "app create rejected the login token"; exit 1; }

DEPLOY_OUT="$("$BIN/zeroship" deploy "$ZSHIP" --app="$APP_ID" --control="$CONTROL_URL" 2>&1)"
DEPLOY_RC=$?
echo "--- zeroship deploy (no --token; it used the saved login): ---"
sed 's/^/  | /' <<<"$DEPLOY_OUT"
echo "-------------------------------------------------------------"
[ "$DEPLOY_RC" = "0" ] && grep -q 'deploy_hash:' <<<"$DEPLOY_OUT" \
  && pass "zeroship deploy succeeded on the saved login credential" \
  || { fail "deploy failed (rc=$DEPLOY_RC)"; exit 1; }

# The gateway routes by Host (`<slug>.<app-base-domain>`), and auth-probe is an
# RPC app with no index route, so "it serves" is checked by calling a procedure
# the app declares `publiclyAccessible` rather than by fetching `/`. Route
# propagation is a control poll (2s) plus a cold worker's first bundle load, so
# this retries on a CONFIRMED success rather than sleeping a guess.
APP_HOST="$APP_SLUG.localhost"
SERVED=""
for _ in $(seq 1 60); do
  CODE="$(curl -sS -o "$WORK/served.json" -w '%{http_code}' -X POST \
    -H "Host: $APP_HOST" -H 'content-type: application/json' \
    "$GATE_URL/__zeroship/v1/probe.public" -d '{"json":{}}' 2>/dev/null || true)"
  if [ "$CODE" = "200" ]; then SERVED=1; break; fi
  sleep 1
done
if [ -n "$SERVED" ]; then
  echo "  POST $GATE_URL/__zeroship/v1/probe.public (Host: $APP_HOST) -> HTTP 200"
  echo "  body: $(head -c 300 "$WORK/served.json")"
  pass "the gateway serves the app deployed with the login token"
else
  fail "the gateway never served $APP_HOST (last HTTP ${CODE:-none}): $(head -c 300 "$WORK/served.json" 2>/dev/null)"
fi

e2e_verdict
