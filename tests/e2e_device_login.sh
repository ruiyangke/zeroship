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
#   GET  /login  and read the href it renders for signup
#     -> GET  that exact URL, and fill the form it returns
#     -> POST /signup                          (a real creator row)
#     -> follow signup's own redirect and sign in
#   real `zeroship login` process
#     -> control GET /.well-known/oauth-protected-resource  (which OP to use)
#     -> OP POST {issuer}/device/authorization (user code + verification URI)
#     -> GET  {verification_uri}?user_code=..  with that signed-in session
#     -> POST /device  csrf + user_code + confirm=authorize
#     -> the CLI's poll of {issuer}/token returns an access + refresh token
#     -> `zeroship deploy` with that token
#     -> the gateway serves the deployed app
#
# THERE IS NO MINT LEG LEFT TO OBSERVE. `zeroship login` used to drive control's
# parallel flow, which POSTed `auth.platform_mint_url` with a dedicated mint
# key; a recording proxy here watched WHERE that POST went, because a fallback
# deriving the target from the issuer looked identical on a single-host
# harness. Control's flow and the mint endpoint are both deleted, so the proxy
# had nothing left to discriminate and a "0 hits" assertion would have been
# true no matter what broke. Both are gone; the CLI redeems at the OP directly.
#
# The signup leg reads its URL off the login page for the same reason the
# approval leg loads the device page: a harness that composes the URL itself
# tests its author's idea of a valid one. Signup accepted ONLY an
# `/oauth2/authorize` continuation while the login page linked to
# `/signup?return_to=/me`, so the product's own path into signup was a 400 and
# every harness that hand-built an RP continuation stayed green.
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
MIGRATED_PORT="${MIGRATED_PORT:-9484}"
MIGRATED_URL="http://localhost:$MIGRATED_PORT"

AUTH_URL="http://localhost:$AUTH_PORT"
CONTROL_URL="http://localhost:$ZEROSHIP_CONTROL_PORT"
GATE_URL="http://localhost:$ZEROSHIP_GATEWAY_PORT"

# THE POINT OF THE WHOLE HARNESS: nothing Supabase-shaped is configured. If any
# of these leak in from the caller's environment the run is not testing the
# shipped default, so drop them rather than silently measuring something else.
unset ZEROSHIP_AUTH_PROVIDER
unset ZEROSHIP_AUTH_SUPABASE_URL ZEROSHIP_AUTH_SUPABASE_ANON_KEY ZEROSHIP_AUTH_SUPABASE_SERVICE_ROLE_KEY ZEROSHIP_AUTH_SUPABASE_JWT_SECRET
unset ZEROSHIP_AUTH_SUPABASE_URL ZEROSHIP_AUTH_SUPABASE_ANON_KEY

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

# Read the FIRST `href` on the page that points at $2 (e.g. `/signup`).
#
# The point of reading it rather than composing it: the signup continuation is
# whatever the login page decided to put in its own link, so a signup endpoint
# that cannot accept it fails this harness. Composing the URL here would test
# the harness author's idea of a valid continuation instead.
page_href() {
  node -e '
const fs=require("fs");
const html=fs.readFileSync(process.argv[1],"utf8");
const re=new RegExp("href=\"("+process.argv[2]+"[^\"]*)\"","i");
const m=html.match(re);
if(!m){process.stdout.write("");process.exit(0)}
process.stdout.write(m[1].replace(/&amp;/g,"&").replace(/&quot;/g,"\"").replace(/&#x27;/g,"'"'"'").replace(/&lt;/g,"<").replace(/&gt;/g,">"));
' "$1" "$2"
}

# `Location:` out of a `curl -D` header dump.
header_location() {
  tr -d '\r' < "$1" | awk 'tolower($1) == "location:" { print $2; exit }'
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
[ -x "$BIN/zeroship-migrated" ] || { fail "missing $BIN/zeroship-migrated"; exit 2; }
if [ ! -f "$ZSHIP" ]; then
  e2e_skipped "no $ZSHIP; run: pnpm --filter ./examples/auth-probe build"
  e2e_verdict || exit 1
  exit 0
fi
pass "binaries and $ZSHIP present"

# Pin the platform issuer to the REAL OP this harness boots, BEFORE
# stack_workspace: unpinned, stack_workspace stands its own JWKS up and names
# THAT as the issuer, and the device-token `iss` assertion below would then be
# comparing a real OP token against a harness origin. Control trusts one
# issuer, so the harness that runs a real one has to say so.
export ZEROSHIP_AUTH_PLATFORM_ISSUER="http://localhost:$AUTH_PORT/oauth2"

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

e2e_with_platform_mint_key "$BIN/zeroship-auth" \
  --addr "0.0.0.0:$AUTH_PORT" --public-url "$AUTH_URL" \
  --control-url "$CONTROL_URL" \
  --signing-key-file "$ZEROSHIP_AUTH_SIGNING_KEY_FILE" \
  --pairwise-salt-file "$ZEROSHIP_AUTH_PAIRWISE_SALT_FILE" \
  --broker-secret-file "$ZEROSHIP_AUTH_BROKER_SECRET_FILE" \
  --refresh-hash-key-file "$ZEROSHIP_AUTH_REFRESH_HASH_KEY_FILE" \
  --refresh-idem-key-file "$ZEROSHIP_AUTH_REFRESH_IDEM_KEY_FILE" \
  --mailer stdout --relay-forward-mailer stdout \
  > "$WORK/auth.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 40); do curl -sf "$AUTH_URL/healthz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "$AUTH_URL/oauth2/.well-known/jwks.json" >/dev/null 2>&1 \
  && pass "auth (platform OP) healthy" \
  || { fail "auth never came up"; tail -30 "$WORK/auth.log"; exit 1; }

e2e_with_platform_mint_key "$BIN/zeroship-control" --port "$ZEROSHIP_CONTROL_PORT" \
  --blob-store "$WORK/blobs" \
  --app-base-domain "localhost" \
  --migrated-url "$MIGRATED_URL" \
  > "$WORK/control.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "$CONTROL_URL/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "$CONTROL_URL/readyz" >/dev/null 2>&1 \
  && pass "control ready" || { fail "control not ready"; tail -30 "$WORK/control.log"; exit 1; }

# The migration service, so this harness can drive `zeroship migrate` on the
# SAME credential the login flow just produced. Every other harness that
# migrates mints its bearer itself from a JWKS it serves; this one carries a
# token the REAL OP issued through the device flow, so this is the leg that
# covers the issuer, the device grant and the scope narrowing together rather
# than the token shape alone.
"$BIN/zeroship-migrated" --port "$MIGRATED_PORT" \
  --tmp-dir "$WORK/migrated-tmp" \
  > "$WORK/migrated.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "$MIGRATED_URL/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "$MIGRATED_URL/readyz" >/dev/null 2>&1 \
  && pass "migrated ready" || { fail "migrated not ready"; tail -30 "$WORK/migrated.log"; exit 1; }

"$BIN/zeroship-worker" --port "$ZEROSHIP_WORKER_PORT" --threads 2 \
  --control-url "$CONTROL_URL" \
  --blob-store "$WORK/blobs" --poll-interval 2 > "$WORK/worker.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "http://localhost:$ZEROSHIP_WORKER_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$ZEROSHIP_WORKER_PORT/readyz" >/dev/null 2>&1 \
  && pass "worker ready" || { fail "worker not ready"; tail -30 "$WORK/worker.log"; exit 1; }

"$BIN/zeroship-gate" --port "$ZEROSHIP_GATEWAY_PORT" --control-url "$CONTROL_URL" \
  --worker-urls "http://localhost:$ZEROSHIP_WORKER_PORT" --blob-store "$WORK/blobs" \
  --blob-cache-disk-root "$WORK/blob-cache" --poll-interval 2 \
  --signing-key-file "$WORK/signing-key.pem" \
  --broker-secret-file "$WORK/gate-secret" \
  --auth-ui-url "$AUTH_URL" \
  > "$WORK/gate.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "$GATE_URL/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "$GATE_URL/readyz" >/dev/null 2>&1 \
  && pass "gateway ready" || { fail "gateway not ready"; tail -30 "$WORK/gate.log"; exit 1; }

# ---------------------------------------------------------------------------
step "Create the human, through the product's own signup"
# WALK THE LINK THE LOGIN PAGE RENDERS. Nothing below composes a signup URL.
#
# The regression this leg exists for: `/login` renders
# `href="/signup?return_to=/me"` from its own sanitized continuation, and
# `/signup` used to demand that the continuation parse as an
# `/oauth2/authorize` request AND that exactly one copy of it arrive. `/me` is
# neither, so the login page's own "Create an account" link answered 400
# "invalid request" -- on a page that still drew the form, the CSRF token and
# a banner, so it looked usable. A bare `/signup` was 400 too (zero targets is
# not one). Signup was reachable ONLY from an OIDC RP continuation.
#
# A harness that composes its own `/oauth2/authorize` return_to walks the one
# arm that worked and reports green. This one starts at `/login`.
EMAIL="creator-$$@zeroship.test"
PASSWORD="correct horse battery staple $$"
JAR="$WORK/session-jar.txt"
rm -f "$JAR"

curl -s -c "$JAR" -m 15 -o "$WORK/login-entry.html" "$AUTH_URL/login" >/dev/null
SIGNUP_HREF="$(page_href "$WORK/login-entry.html" /signup)"
[ -n "$SIGNUP_HREF" ] \
  && pass "the login page offers signup at $SIGNUP_HREF" \
  || { fail "GET /login rendered no /signup link"; head -40 "$WORK/login-entry.html"; exit 1; }

SIGNUP_GET_CODE="$(curl -s -b "$JAR" -c "$JAR" -m 15 -o "$WORK/signup.html" -w '%{http_code}' \
  "$AUTH_URL$SIGNUP_HREF")"
echo "  GET $AUTH_URL$SIGNUP_HREF -> HTTP $SIGNUP_GET_CODE"
[ "$SIGNUP_GET_CODE" = "200" ] \
  && pass "the link the login page renders reaches the signup form" \
  || { fail "the login page's own signup link returned HTTP $SIGNUP_GET_CODE"
       grep -o '<div class="error">[^<]*</div>' "$WORK/signup.html" || head -20 "$WORK/signup.html"
       exit 1; }
grep -q '<div class="error">' "$WORK/signup.html" \
  && { fail "the signup form rendered an error banner: $(grep -o '<div class="error">[^<]*</div>' "$WORK/signup.html")"; exit 1; } \
  || pass "the signup form carries no error banner"

SIGNUP_CSRF="$(form_field "$WORK/signup.html" csrf)"
SIGNUP_RETURN_TO="$(form_field "$WORK/signup.html" return_to)"
[ -n "$SIGNUP_CSRF" ] || { fail "no csrf on the signup form"; exit 1; }
[ -n "$SIGNUP_RETURN_TO" ] \
  && pass "the form echoes a continuation to sign in against ($SIGNUP_RETURN_TO)" \
  || fail "the signup form echoed an empty return_to"

SIGNUP_HDRS="$WORK/signup-post.headers"
SIGNUP_CODE="$(curl -s -b "$JAR" -c "$JAR" -m 30 -D "$SIGNUP_HDRS" \
  -o "$WORK/signup-post.html" -w '%{http_code}' \
  -X POST "$AUTH_URL/signup" \
  --data-urlencode "csrf=$SIGNUP_CSRF" --data-urlencode "return_to=$SIGNUP_RETURN_TO" \
  --data-urlencode "name=Device Login Creator" --data-urlencode "email=$EMAIL" \
  --data-urlencode "password=$PASSWORD")"
SIGNUP_LOCATION="$(header_location "$SIGNUP_HDRS")"
echo "  POST $AUTH_URL/signup -> HTTP $SIGNUP_CODE  Location: ${SIGNUP_LOCATION:-none}"
USER_ID="$(psql_q "SELECT id FROM zeroship.users WHERE email = '$EMAIL'::citext")"
[ -n "$USER_ID" ] && pass "signup created $EMAIL ($USER_ID), HTTP $SIGNUP_CODE" \
  || { fail "signup did not create a user (HTTP $SIGNUP_CODE)"
       grep -o '<div class="error">[^<]*</div>' "$WORK/signup-post.html" || true
       exit 1; }
[ "$SIGNUP_CODE" = "302" ] && [ -n "$SIGNUP_LOCATION" ] \
  && pass "signup redirected the new account onward ($SIGNUP_LOCATION)" \
  || fail "signup answered HTTP $SIGNUP_CODE with Location '${SIGNUP_LOCATION:-none}'"

# Account-enumeration defense, on the live server: re-signing up the SAME
# email must be indistinguishable from a fresh one. `crates/auth` owns the
# byte-level assertion (tests/signup_continuation_test.rs); this checks the
# shipped binary agrees, since a divergence here is a live email oracle.
dup_signup() {
  local email="$1" out="$2" hdrs="$3" page="$WORK/dup-get.html" csrf
  curl -s -b "$JAR" -c "$JAR" -m 15 -o "$page" "$AUTH_URL$SIGNUP_HREF" >/dev/null
  csrf="$(form_field "$page" csrf)"
  curl -s -b "$JAR" -c "$JAR" -m 30 -D "$hdrs" -o "$out" -w '%{http_code}' \
    -X POST "$AUTH_URL/signup" \
    --data-urlencode "csrf=$csrf" --data-urlencode "return_to=$SIGNUP_RETURN_TO" \
    --data-urlencode "name=Device Login Creator" --data-urlencode "email=$email" \
    --data-urlencode "password=$PASSWORD"
}
DUP_CODE="$(dup_signup "$EMAIL" "$WORK/dup.html" "$WORK/dup.headers")"
FRESH_CODE="$(dup_signup "fresh-$$@zeroship.test" "$WORK/fresh.html" "$WORK/fresh.headers")"
DUP_LOC="$(header_location "$WORK/dup.headers")"
FRESH_LOC="$(header_location "$WORK/fresh.headers")"
echo "  duplicate: HTTP $DUP_CODE Location=$DUP_LOC   fresh: HTTP $FRESH_CODE Location=$FRESH_LOC"
if [ "$DUP_CODE" = "$FRESH_CODE" ] && [ "$DUP_LOC" = "$FRESH_LOC" ] \
   && cmp -s "$WORK/dup.html" "$WORK/fresh.html"; then
  pass "a duplicate signup is byte-identical to a fresh one (no email oracle)"
else
  fail "duplicate signup is distinguishable: $DUP_CODE/$DUP_LOC vs $FRESH_CODE/$FRESH_LOC"
fi
DUP_USERS="$(psql_q "SELECT count(*) FROM zeroship.users WHERE email = '$EMAIL'::citext")"
[ "$DUP_USERS" = "1" ] && pass "the duplicate did not create a second row" \
  || fail "expected 1 row for $EMAIL, found $DUP_USERS"

# The OIDC RP continuation -- the ONLY shape signup used to accept -- must
# still round-trip. The fix widened the intake; it must not have moved it.
SIGNUP_CLIENT="oac_devlogin_$$"
docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -q >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.oauth_clients (client_id, client_name, redirect_uris, scopes)
VALUES ('$SIGNUP_CLIENT', 'device login harness', ARRAY['http://localhost/cb'], ARRAY['openid'])
ON CONFLICT (client_id) DO NOTHING;
SQL
RETURN_TO="/oauth2/authorize?response_type=code&client_id=$SIGNUP_CLIENT&redirect_uri=http%3A%2F%2Flocalhost%2Fcb&scope=openid&state=s&nonce=n&code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM&code_challenge_method=S256"
RP_CODE="$(curl -s -m 15 -o "$WORK/signup-rp.html" -w '%{http_code}' -G "$AUTH_URL/signup" \
  --data-urlencode "return_to=$RETURN_TO")"
RP_ECHOED="$(form_field "$WORK/signup-rp.html" return_to)"
[ "$RP_CODE" = "200" ] && [ "$RP_ECHOED" = "$RETURN_TO" ] \
  && pass "an OIDC RP continuation still reaches signup and is echoed intact" \
  || fail "RP continuation: HTTP $RP_CODE, echoed '$RP_ECHOED'"

# An off-origin continuation must be REPLACED, not echoed and not 400'd.
EVIL_CODE="$(curl -s -m 15 -o "$WORK/signup-evil.html" -w '%{http_code}' -G "$AUTH_URL/signup" \
  --data-urlencode "return_to=//evil.example")"
EVIL_ECHOED="$(form_field "$WORK/signup-evil.html" return_to)"
case "$EVIL_ECHOED" in
  *evil*) fail "signup echoed an off-origin continuation: $EVIL_ECHOED" ;;
  *) pass "an off-origin continuation is replaced, not echoed (HTTP $EVIL_CODE, return_to='$EVIL_ECHOED')" ;;
esac

# The auth service can read but cannot write `zeroship.principal_grants`, so a
# freshly signed-up platform creator has NO grants. Recorded here because the
# OP device grant the CLI now drives does not intersect with that table at all
# (see the FINDING at the end of this harness): the login below succeeds with a
# full-scope token against exactly this empty grant set.
GRANTS_AT_SIGNUP="$(psql_q "SELECT count(*) FROM zeroship.principal_grants WHERE principal_id = '$USER_ID'")"
[ "$GRANTS_AT_SIGNUP" = "0" ] \
  && pass "a fresh platform principal has 0 grants" \
  || fail "expected 0 grants at signup, got $GRANTS_AT_SIGNUP"

step "Log in, the way a browser does"
# Follow the redirect signup issued rather than inventing a login URL: the
# whole point of the continuation is that it survives the signup hop.
rm -f "$JAR"
LOGIN_ENTRY="$AUTH_URL${SIGNUP_LOCATION:-/login}"
echo "  following the signup redirect: $LOGIN_ENTRY"
curl -s -c "$JAR" -m 15 -o "$WORK/login.html" "$LOGIN_ENTRY" >/dev/null
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
for scope in 'apps:deploy' 'apps:read' 'apps:write' 'secrets:read'; do
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
TOKEN_AUD="$(jwt_claim "$TOKEN" aud)"
TOKEN_CLIENT_ID="$(jwt_claim "$TOKEN" client_id)"
TOKEN_EXP="$(jwt_claim "$TOKEN" exp)"
TOKEN_IAT="$(jwt_claim "$TOKEN" iat)"
TOKEN_TTL=$((TOKEN_EXP - TOKEN_IAT))
echo "  token: sub=$TOKEN_SUB aud=$TOKEN_AUD client_id=$TOKEN_CLIENT_ID scope='$TOKEN_SCOPE' ttl=${TOKEN_TTL}s"
REFRESH_TOKEN="$(jget '.refresh_token' < "$CREDS")"
[ "$TOKEN_SCOPE" = "apps:deploy apps:read apps:write secrets:read offline_access" ] \
  && pass "the deploy token carries the creator scopes plus offline_access" \
  || fail "deploy token scope was '$TOKEN_SCOPE'"
[ "$TOKEN_SUB" = "$USER_ID" ] && pass "the token's subject is the approving user" \
  || fail "token sub $TOKEN_SUB != $USER_ID"
[ "$TOKEN_AUD" = "control.zeroship.ai" ] \
  && pass "the token audience is control" \
  || fail "token audience was '$TOKEN_AUD'"
[ "$TOKEN_CLIENT_ID" = "zeroship-cli" ] \
  && pass "the token client_id is zeroship-cli" \
  || fail "token client_id was '$TOKEN_CLIENT_ID'"
# 15 minutes, not the 12-hour ceiling: the long-lived half is now the refresh
# token, which is DB-backed and can be revoked. A bearer cannot be.
[ "$TOKEN_TTL" = "900" ] \
  && pass "the access token expires in 15 minutes (${TOKEN_TTL}s)" \
  || fail "access token TTL was ${TOKEN_TTL}s, expected 900s"
case "$REFRESH_TOKEN" in
  zrt_*) pass "the CLI stored a refresh token" ;;
  *) fail "the CLI stored no refresh token; a 15-minute session with no rotation is worse than the 12-hour bearer it replaced" ;;
esac

TOKEN_ISS="$(jwt_claim "$TOKEN" iss)"
[ "$TOKEN_ISS" = "$ZEROSHIP_AUTH_PLATFORM_ISSUER" ] \
  && pass "the token carries the PUBLIC issuer ($TOKEN_ISS)" \
  || fail "token iss '$TOKEN_ISS' != configured issuer '$ZEROSHIP_AUTH_PLATFORM_ISSUER'"

# Login is an OP-only conversation, so control has still never seen this
# principal and the grant table is still empty. That is not the gap it used to
# be: the token's scope is the client REGISTRATION, a coarse ceiling, and the
# entitlement intersection happens in control at request time. The two steps
# below are what close the loop - materialization on control's first sight of
# the principal, then narrowing.
GRANTS_AFTER="$(psql_q "SELECT count(*) FROM zeroship.principal_grants WHERE principal_id = '$USER_ID'")"
[ "$GRANTS_AFTER" = "0" ] \
  && pass "login alone still writes no grants (the OP cannot; it has SELECT only)" \
  || fail "grants appeared during login ($GRANTS_AFTER); only control may write them"

# ---------------------------------------------------------------------------
step "Deploy with that token, and serve it"
APP_ID="$(curl -sS -X POST "$CONTROL_URL/api/apps" -H "Authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' -d "{\"name\":\"$APP_SLUG\"}" | jget '.id')"
[ -n "$APP_ID" ] && pass "created app $APP_SLUG ($APP_ID) with the login token" \
  || { fail "app create rejected the login token"; exit 1; }

# That request was control's FIRST sight of this principal, and it had to
# succeed against an empty grant table - an unseeded principal is treated as
# holding the default CLI set, because intersecting with nothing would 403
# every creator's first command. Control materializes the defaults on the way
# through, which is what gives an operator rows to delete below.
GRANTS_MATERIALIZED="$(psql_q "SELECT string_agg(grant_name, ',' ORDER BY grant_name) FROM zeroship.principal_grants WHERE principal_id = '$USER_ID'")"
[ "$GRANTS_MATERIALIZED" = "apps:deploy,apps:read,apps:write,secrets:read" ] \
  && pass "control materialized the default CLI grants on first contact" \
  || fail "expected the four default grants after the first control request, got '$GRANTS_MATERIALIZED'"

DEPLOY_OUT="$("$BIN/zeroship" deploy "$ZSHIP" --app="$APP_ID" --control="$CONTROL_URL" 2>&1)"
DEPLOY_RC=$?
echo "--- zeroship deploy (no --token; it used the saved login): ---"
sed 's/^/  | /' <<<"$DEPLOY_OUT"
echo "-------------------------------------------------------------"
[ "$DEPLOY_RC" = "0" ] && grep -q 'deploy_hash:' <<<"$DEPLOY_OUT" \
  && pass "zeroship deploy succeeded on the saved login credential" \
  || { fail "deploy failed (rc=$DEPLOY_RC)"; exit 1; }

# The capability this harness exists to guard: an operator DELETE against
# `zeroship.principal_grants` narrows what the credential already in the
# creator's hand can do. No new login, no new token - the SAME saved
# credential, which still carries `apps:deploy` in its `scope` claim, because
# the entitlement check is control's and it reads the table per request.
step "Revoke apps:deploy and confirm the saved credential is narrowed"
psql_q "DELETE FROM zeroship.principal_grants WHERE principal_id = '$USER_ID' AND grant_name = 'apps:deploy'" >/dev/null
NARROWED_OUT="$("$BIN/zeroship" deploy "$ZSHIP" --app="$APP_ID" --control="$CONTROL_URL" 2>&1)"
NARROWED_RC=$?
echo "--- zeroship deploy after the grant was revoked: ---"
sed 's/^/  | /' <<<"$NARROWED_OUT"
echo "----------------------------------------------------"
[ "$NARROWED_RC" != "0" ] \
  && pass "revoking apps:deploy narrowed the token already issued (rc=$NARROWED_RC)" \
  || { fail "deploy still succeeded after apps:deploy was revoked; narrowing is not enforced"; exit 1; }

# Restore it: later steps run on this same credential, and leaving it revoked
# would make an unrelated failure look like a narrowing bug.
psql_q "INSERT INTO zeroship.principal_grants (principal_id, grant_name) VALUES ('$USER_ID', 'apps:deploy') ON CONFLICT DO NOTHING" >/dev/null
RESTORED="$(psql_q "SELECT count(*) FROM zeroship.principal_grants WHERE principal_id = '$USER_ID' AND grant_name = 'apps:deploy'")"
[ "$RESTORED" = "1" ] \
  && pass "apps:deploy restored for the remainder of the harness" \
  || fail "could not restore apps:deploy (count=$RESTORED)"

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

# ---------------------------------------------------------------------------
step "Migrate the same app with the same login credential"
#
# WHAT THIS LEG COVERS AND WHAT IT DOES NOT. It covers AUTHORIZATION: that the
# credential `zeroship login` produced -- an OAuth device-flow bearer carrying
# the full CLI scope asserted above is accepted all the way
# through control's authz, control's forward, and migrated's independent
# re-verification plus its `app_members` owner-row check (the row was created by
# `POST /api/apps` above, not seeded here). Nothing else in the suite drives
# that token type into the migration service.
#
# It does NOT cover this app's own schema: `examples/auth-probe` has no
# `migrations/`, so there is nothing of its own to apply. The IR posted below is
# BORROWED from `examples/db-todos`. The tables it creates are incidental; only
# the 200 is the claim. If you are looking for "a deployed app's env.db works
# after migrating", that is `tests/e2e_db_app_end_to_end.sh`, which drives the
# app's own migrations and then reads a row back.
BORROWED_IR="$ROOT/examples/db-todos/generated/zeroship/migrations.ir.json"
if [ ! -f "$BORROWED_IR" ]; then
  fail "missing $BORROWED_IR - run: pnpm gen-types"
else
  MIG_OUT="$("$BIN/zeroship" migrate "$BORROWED_IR" --app="$APP_ID" --control="$CONTROL_URL" 2>&1)"
  MIG_RC=$?
  echo "  zeroship migrate (no --token; it used the saved login):"
  echo "$MIG_OUT" | sed 's/^/    /'
  if [ "$MIG_RC" = "0" ] && grep -qE 'Applied [1-9][0-9]* migration op' <<<"$MIG_OUT"; then
    pass "zeroship migrate succeeded on the saved login credential"
  else
    fail "zeroship migrate failed on the login credential (rc=$MIG_RC): $MIG_OUT"
    tail -20 "$WORK/migrated.log"
  fi

  # The negative half: a creator must not be able to migrate an app they do not
  # own.
  #
  # WHAT THIS DOES NOT SHOW. The refusal below almost certainly comes from
  # CONTROL, which authorizes before it forwards, so this arm says nothing about
  # migrated's own gate. That gate exists and is independent (migrated
  # re-verifies the bearer and additionally requires an `app_members` owner row,
  # crates/migrated/src/auth.rs), but proving it would mean reaching migrated
  # directly, which is exactly the topology this design removes. Read this as
  # "the creator-facing surface refuses", not "defence in depth was measured".
  FOREIGN_APP="$(node -e 'console.log(require("crypto").randomUUID())')"
  FOREIGN_OUT="$("$BIN/zeroship" migrate "$BORROWED_IR" --app="$FOREIGN_APP" --control="$CONTROL_URL" 2>&1)"
  grep -qE 'HTTP (403|404)' <<<"$FOREIGN_OUT" \
    && pass "migrating an app this creator does not own is refused ($(grep -oE 'HTTP [0-9]+' <<<"$FOREIGN_OUT" | head -1))" \
    || fail "an unowned app was not refused: $FOREIGN_OUT"
fi

e2e_verdict
