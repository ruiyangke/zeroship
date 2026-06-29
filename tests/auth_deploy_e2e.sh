#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Production deploy auth e2e:
#   zeroship login (OAuth Device Authorization Grant against Hydra)
#     -> Hydra access_token saved by the CLI
#     -> zeroship deploy with that bearer
#     -> control introspects the token and deploys
#     -> gateway serves the deployed starter app
#
# Route B: isolated Postgres + Hydra containers, local release binaries for
# auth/control/worker/gateway/CLI. No dev-provision path is used.
# ---------------------------------------------------------------------------
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release"
STARTER="$ROOT/examples/starter"
ZSHIP="$STARTER/dist/app.zship"

PG_CONTAINER="${PG_CONTAINER:-zeroship-auth-deploy-pg}"
HYDRA_CONTAINER="${HYDRA_CONTAINER:-zeroship-auth-deploy-hydra}"
NETWORK="${NETWORK:-zeroship-auth-deploy-net}"

PG_PORT="${PG_PORT:-5540}"
HYDRA_PUBLIC_PORT="${HYDRA_PUBLIC_PORT:-4544}"
HYDRA_ADMIN_PORT="${HYDRA_ADMIN_PORT:-4545}"
AUTH_PORT="${AUTH_PORT:-9192}"
CONTROL_PORT="${CONTROL_PORT:-9490}"
WORKER_PORT="${WORKER_PORT:-8490}"
GATE_PORT="${GATE_PORT:-8400}"

HYDRA_IMAGE="${HYDRA_IMAGE:-oryd/hydra:v25.4.0}"
OAUTH_AUDIENCE="${OAUTH_AUDIENCE:-control.zeroship.ai}"
APP_NAME="${APP_NAME:-oauth-starter}"
TEST_PASSWORD="${TEST_PASSWORD:-correct horse battery staple 2026}"

CONTROL_KEY="${CONTROL_KEY:-auth-deploy-control-key}"
MASTER_KEY="${MASTER_KEY:-auth-deploy-master-key-0123456789abcdef}"
WORKER_KEY="${WORKER_KEY:-auth-deploy-worker-key-0123456789abcdef}"

WORKDIR="${WORKDIR:-$(mktemp -d /tmp/zeroship-auth-deploy.XXXXXX)}"
BUNDLES="$WORKDIR/bundles"
APP_STORAGE="$WORKDIR/app-storage"
CONFIG_HOME="$WORKDIR/cli-config"
HYDRA_CONFIG="$WORKDIR/hydra.yaml"
CLIENTS_CONFIG="$WORKDIR/auth-clients.toml"

DB_URL="postgres://postgres:zeroship@127.0.0.1:${PG_PORT}/zeroship"
HYDRA_PUBLIC_URL="http://127.0.0.1:${HYDRA_PUBLIC_PORT}"
HYDRA_ADMIN_URL="http://127.0.0.1:${HYDRA_ADMIN_PORT}"
AUTH_URL="http://127.0.0.1:${AUTH_PORT}"
CONTROL_URL="http://127.0.0.1:${CONTROL_PORT}"
GATE_URL="http://127.0.0.1:${GATE_PORT}"

PASS=0
FAIL=0
PIDS=()

step() { printf '\n=== %s ===\n' "$1"; }
pass() { PASS=$((PASS + 1)); printf '  [ok] %s\n' "$1"; }
fail() {
  FAIL=$((FAIL + 1))
  printf '  [fail] %s\n' "$1" >&2
  dump_logs
  exit 1
}

dump_file_tail() {
  local label="$1" file="$2"
  if [ -f "$file" ]; then
    printf '\n--- %s (%s) ---\n' "$label" "$file" >&2
    tail -80 "$file" >&2 || true
  fi
}

dump_logs() {
  dump_file_tail "login" "$WORKDIR/login.log"
  dump_file_tail "deploy" "$WORKDIR/deploy.log"
  dump_file_tail "signup post headers" "$WORKDIR/signup-post.headers"
  dump_file_tail "signup post body" "$WORKDIR/signup-post.html"
  dump_file_tail "device post headers" "$WORKDIR/device-post.headers"
  dump_file_tail "device post body" "$WORKDIR/device-post.html"
  dump_file_tail "device complete" "$WORKDIR/device-complete.html"
  dump_file_tail "browser login headers" "$WORKDIR/browser-login.headers"
  dump_file_tail "browser login body" "$WORKDIR/browser-login.html"
  dump_file_tail "auth" "$WORKDIR/auth.log"
  dump_file_tail "control" "$WORKDIR/control.log"
  dump_file_tail "worker" "$WORKDIR/worker.log"
  dump_file_tail "gateway" "$WORKDIR/gateway.log"
  if docker ps -a --format '{{.Names}}' | grep -qx "$HYDRA_CONTAINER"; then
    printf '\n--- hydra container logs ---\n' >&2
    docker logs --tail 120 "$HYDRA_CONTAINER" >&2 || true
  fi
}

cleanup() {
  local status=$?
  for pid in "${PIDS[@]:-}"; do
    kill "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
  docker rm -f "$HYDRA_CONTAINER" "$PG_CONTAINER" >/dev/null 2>&1 || true
  docker network rm "$NETWORK" >/dev/null 2>&1 || true
  if [ "${KEEP_AUTH_DEPLOY_E2E:-0}" != "1" ]; then
    rm -rf "$WORKDIR"
  else
    printf 'Keeping workdir: %s\n' "$WORKDIR"
  fi
  exit "$status"
}
trap cleanup EXIT

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

new_uuid() {
  if [ -r /proc/sys/kernel/random/uuid ]; then
    tr '[:upper:]' '[:lower:]' < /proc/sys/kernel/random/uuid
  else
    uuidgen | tr '[:upper:]' '[:lower:]'
  fi
}

require_port_free() {
  local port="$1"
  if command -v lsof >/dev/null 2>&1 && lsof -ti :"$port" >/dev/null 2>&1; then
    fail "port $port is already in use"
  fi
}

wait_for() {
  local label="$1" timeout="$2"
  shift 2
  local start=$SECONDS
  until "$@" >/dev/null 2>&1; do
    if [ $((SECONDS - start)) -ge "$timeout" ]; then
      fail "timed out waiting for $label"
    fi
    sleep 1
  done
  pass "$label ready"
}

psql_exec() {
  docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 "$@"
}

ensure_release_bins() {
  local missing=()
  for bin in zeroship zeroship-auth zeroship-control zeroship-worker zeroship-gate; do
    [ -x "$BIN/$bin" ] || missing+=("$bin")
  done
  if [ "${#missing[@]}" -eq 0 ]; then
    pass "release binaries present"
    return
  fi

  command -v nix >/dev/null 2>&1 || fail "missing release binaries (${missing[*]}) and nix is not installed"
  step "Build missing release binaries (${missing[*]})"
  nix develop --command cargo build --release \
    -p zeroship \
    -p zeroship-auth \
    -p zeroship-control \
    -p zeroship-worker \
    -p zeroship-gateway
  pass "built missing release binaries"
}

write_hydra_config() {
  cat > "$HYDRA_CONFIG" <<EOF
serve:
  public:
    port: 4444
    host: 0.0.0.0
  admin:
    port: 4445
    host: 0.0.0.0
  cookies:
    same_site_mode: Lax

urls:
  self:
    issuer: ${HYDRA_PUBLIC_URL}
    public: ${HYDRA_PUBLIC_URL}
  login:   ${AUTH_URL}/login
  consent: ${AUTH_URL}/consent
  logout:  ${AUTH_URL}/logout
  error:   ${AUTH_URL}/error

strategies:
  access_token: opaque

oauth2:
  pkce:
    enforced_for_public_clients: true
    enforced: true
  grant:
    refresh_token:
      rotation_grace_period: 30s
      rotation_grace_reuse_count: 3

ttl:
  access_token: 1h
  refresh_token: 720h
  id_token: 1h
  auth_code: 60s
  login_consent_request: 1h

oidc:
  subject_identifiers:
    supported_types: [public]
  dynamic_client_registration:
    enabled: false

log:
  level: info
  format: json
EOF
}

write_clients_config() {
  cat > "$CLIENTS_CONFIG" <<EOF
[[client]]
client_id = "zeroship-cli"
client_name = "zeroship CLI"
client_secret = ""
grant_types = ["urn:ietf:params:oauth:grant-type:device_code", "refresh_token"]
response_types = []
redirect_uris = []
scope = "openid offline_access apps:read apps:write apps:deploy"
token_endpoint_auth_method = "none"
audience = ["${OAUTH_AUDIENCE}"]
first_party = true
EOF
}

extract_csrf_cookie() {
  local headers="$1"
  tr -d '\r' < "$headers" | awk '
    BEGIN { IGNORECASE = 1 }
    /^set-cookie:[[:space:]]*zsidp_csrf=/ {
      sub(/^set-cookie:[[:space:]]*zsidp_csrf=/, "")
      sub(/;.*/, "")
      print
      exit
    }
  '
}

extract_login_user_code() {
  local log="$1"
  awk -F'Enter code: ' '/Enter code:/ { print $2 }' "$log" | tail -1 | tr -d '[:space:]'
}

extract_hidden_value() {
  local name="$1" file="$2"
  sed -nE "s/.*name=\"${name}\" value=\"([^\"]*)\".*/\\1/p" "$file" | head -1
}

extract_form_action() {
  local file="$1"
  sed -nE 's/.*<form method="POST" action="([^"]+)".*/\1/p' "$file" \
    | head -1 \
    | sed 's/&amp;/\&/g'
}

absolute_auth_url() {
  local url="$1"
  case "$url" in
    http://*|https://*) printf '%s\n' "$url" ;;
    /*) printf '%s%s\n' "$AUTH_URL" "$url" ;;
    *) fail "cannot resolve relative auth URL: $url" ;;
  esac
}

wait_for_login_code() {
  local login_pid="$1" log="$2"
  local start=$SECONDS
  local code=""
  while [ $((SECONDS - start)) -lt 60 ]; do
    code="$(extract_login_user_code "$log")"
    if [ -n "$code" ]; then
      printf '%s\n' "$code"
      return 0
    fi
    if ! kill -0 "$login_pid" 2>/dev/null; then
      return 1
    fi
    sleep 1
  done
  return 1
}

wait_for_login_exit() {
  local login_pid="$1"
  local start=$SECONDS
  while kill -0 "$login_pid" 2>/dev/null; do
    if [ $((SECONDS - start)) -ge 90 ]; then
      kill "$login_pid" 2>/dev/null || true
      wait "$login_pid" 2>/dev/null || true
      return 124
    fi
    sleep 1
  done
  wait "$login_pid"
}

printf '============================================\n'
printf '  zeroship OAuth device login -> deploy e2e\n'
printf '============================================\n'
printf 'Route: B (direct binaries + Hydra container)\n'
printf 'Workdir: %s\n' "$WORKDIR"

for cmd in docker curl jq pnpm awk sed grep; do
  require_cmd "$cmd"
done
for port in "$PG_PORT" "$HYDRA_PUBLIC_PORT" "$HYDRA_ADMIN_PORT" "$AUTH_PORT" "$CONTROL_PORT" "$WORKER_PORT" "$GATE_PORT"; do
  require_port_free "$port"
done
mkdir -p "$BUNDLES" "$APP_STORAGE" "$CONFIG_HOME"
ensure_release_bins

step "Build starter .zship"
( cd "$STARTER" && pnpm build ) >"$WORKDIR/starter-build.log" 2>&1 \
  || { dump_file_tail "starter build" "$WORKDIR/starter-build.log"; fail "examples/starter build failed"; }
[ -f "$ZSHIP" ] || fail "starter build produced no dist/app.zship"
pass "built examples/starter/dist/app.zship ($(du -k "$ZSHIP" | awk '{print $1}')KB)"

step "Start isolated Postgres"
docker rm -f "$HYDRA_CONTAINER" "$PG_CONTAINER" >/dev/null 2>&1 || true
docker network rm "$NETWORK" >/dev/null 2>&1 || true
docker network create "$NETWORK" >/dev/null
docker run -d \
  --name "$PG_CONTAINER" \
  --network "$NETWORK" \
  -e POSTGRES_PASSWORD=zeroship \
  -e POSTGRES_DB=zeroship \
  -p "127.0.0.1:${PG_PORT}:5432" \
  postgres:16 >/dev/null
wait_for "postgres" 60 docker exec "$PG_CONTAINER" pg_isready -U postgres
wait_for "postgres SQL" 60 psql_exec -tAc "select 1"
psql_exec -q -c "ALTER ROLE postgres SET search_path = zeroship, public" >/dev/null

step "Apply platform migrations"
for f in $(find "$ROOT/db/migrations" -maxdepth 1 -name 'V*.sql' ! -name '*.down.sql' | sort); do
  psql_exec -q < "$f" >"$WORKDIR/migrate.log" 2>&1 \
    || { dump_file_tail "migration $(basename "$f")" "$WORKDIR/migrate.log"; fail "platform migration failed: $(basename "$f")"; }
done
psql_exec -tAc "select to_regclass('zeroship.apps')" | grep -q apps \
  || fail "platform schema missing zeroship.apps"
pass "platform schema migrated"

step "Start Hydra"
write_hydra_config
docker run --rm \
  --network "$NETWORK" \
  -e "DSN=postgres://oauth_hydra:zeroship@${PG_CONTAINER}:5432/zeroship?sslmode=disable" \
  "$HYDRA_IMAGE" migrate sql up -e --yes >"$WORKDIR/hydra-migrate.log" 2>&1 \
  || { dump_file_tail "hydra migrate" "$WORKDIR/hydra-migrate.log"; fail "hydra migration failed"; }
docker run -d \
  --name "$HYDRA_CONTAINER" \
  --network "$NETWORK" \
  -e "DSN=postgres://oauth_hydra:zeroship@${PG_CONTAINER}:5432/zeroship?sslmode=disable" \
  -e "SECRETS_SYSTEM=dev-secret-please-change-this-please" \
  -e "SECRETS_COOKIE=dev-secret-please-change-this-please" \
  -v "$HYDRA_CONFIG:/etc/config/hydra.yaml:ro" \
  -p "127.0.0.1:${HYDRA_PUBLIC_PORT}:4444" \
  -p "127.0.0.1:${HYDRA_ADMIN_PORT}:4445" \
  "$HYDRA_IMAGE" serve all --dev --config /etc/config/hydra.yaml >/dev/null
wait_for "hydra public" 60 curl -fsS "$HYDRA_PUBLIC_URL/health/ready"
wait_for "hydra admin" 60 curl -fsS "$HYDRA_ADMIN_URL/health/ready"

step "Start auth and register zeroship-cli client"
write_clients_config
"$BIN/zeroship-auth" \
  --no-config \
  --addr "127.0.0.1:${AUTH_PORT}" \
  --db-url "$DB_URL" \
  --hydra-admin-url "$HYDRA_ADMIN_URL" \
  --hydra-public-url "$HYDRA_PUBLIC_URL" \
  --public-url "$AUTH_URL" \
  --clients-config "$CLIENTS_CONFIG" \
  --bootstrap \
  --dev-insecure \
  --relay-forward-mailer stdout \
  >"$WORKDIR/auth.log" 2>&1 &
PIDS+=("$!")
wait_for "auth" 60 curl -fsS "$AUTH_URL/healthz"
CLIENT_JSON="$(curl -fsS "$HYDRA_ADMIN_URL/admin/clients/zeroship-cli")"
echo "$CLIENT_JSON" | jq -e \
  --arg aud "$OAUTH_AUDIENCE" \
  '.grant_types | index("urn:ietf:params:oauth:grant-type:device_code")' >/dev/null \
  || fail "zeroship-cli client missing device grant"
echo "$CLIENT_JSON" | jq -e --arg aud "$OAUTH_AUDIENCE" '.audience | index($aud)' >/dev/null \
  || fail "zeroship-cli client missing expected audience"
echo "$CLIENT_JSON" | jq -e \
  '(.scope // "" | split(" ") | index("apps:write") != null)
   and (.scope // "" | split(" ") | index("apps:deploy") != null)' >/dev/null \
  || fail "zeroship-cli client missing apps:write/apps:deploy scope"
pass "Hydra client zeroship-cli registered for device grant + apps:write/apps:deploy scopes + ${OAUTH_AUDIENCE} audience"

step "Start control, worker, and gateway"
"$BIN/zeroship-control" \
  --no-config \
  --port "$CONTROL_PORT" \
  --db "$DB_URL" \
  --provision-db "$DB_URL" \
  --blob-store "$BUNDLES" \
  --control-key "$CONTROL_KEY" \
  --master-key "$MASTER_KEY" \
  --worker-key "$WORKER_KEY" \
  --hydra-admin-url "$HYDRA_ADMIN_URL" \
  --hydra-public-url "$HYDRA_PUBLIC_URL" \
  --oauth-audience "$OAUTH_AUDIENCE" \
  --app-base-domain zeroship.localhost \
  --dev-insecure \
  >"$WORKDIR/control.log" 2>&1 &
PIDS+=("$!")
wait_for "control" 60 curl -fsS "$CONTROL_URL/health"

"$BIN/zeroship-worker" \
  --no-config \
  --port "$WORKER_PORT" \
  --worker-threads 2 \
  --control "$CONTROL_URL" \
  --control-key "$CONTROL_KEY" \
  --worker-key "$WORKER_KEY" \
  --db "$DB_URL" \
  --blob-store "$BUNDLES" \
  --storage-url "$APP_STORAGE" \
  --poll-interval 2 \
  --dev-insecure \
  >"$WORKDIR/worker.log" 2>&1 &
PIDS+=("$!")
wait_for "worker" 60 curl -fsS "http://127.0.0.1:${WORKER_PORT}/health"

"$BIN/zeroship-gate" \
  --no-config \
  --port "$GATE_PORT" \
  --control "$CONTROL_URL" \
  --control-key "$CONTROL_KEY" \
  --workers "http://127.0.0.1:${WORKER_PORT}" \
  --worker-key "$WORKER_KEY" \
  --blob-store "$BUNDLES" \
  --db "$DB_URL" \
  --hydra-public-url "$HYDRA_PUBLIC_URL" \
  --auth-ui-url "$AUTH_URL" \
  --gateway-public-url "$GATE_URL" \
  --poll-interval 2 \
  --dev-insecure \
  >"$WORKDIR/gateway.log" 2>&1 &
PIDS+=("$!")
wait_for "gateway" 60 curl -fsS "$GATE_URL/health"

step "Seed signed-in auth user for device approval"
SESSION_ID="$(new_uuid)"
TEST_EMAIL="oauth-deploy-$(new_uuid)@zeroship.test"

SIGNUP_COOKIE_JAR="$WORKDIR/signup.cookies"
curl -fsS -D "$WORKDIR/signup.headers" -c "$SIGNUP_COOKIE_JAR" -o "$WORKDIR/signup.html" "$AUTH_URL/signup"
SIGNUP_CSRF="$(extract_hidden_value csrf "$WORKDIR/signup.html")"
[ -n "$SIGNUP_CSRF" ] || fail "GET /signup did not render a csrf token"
SIGNUP_STATUS="$(curl -sS -D "$WORKDIR/signup-post.headers" -b "$SIGNUP_COOKIE_JAR" -c "$SIGNUP_COOKIE_JAR" \
  -o "$WORKDIR/signup-post.html" -w '%{http_code}' \
  -H "Content-Type: application/x-www-form-urlencoded" \
  "$AUTH_URL/signup" \
  --data-urlencode "name=OAuth Deploy User" \
  --data-urlencode "email=${TEST_EMAIL}" \
  --data-urlencode "password=${TEST_PASSWORD}" \
  --data-urlencode "csrf=${SIGNUP_CSRF}")"
case "$SIGNUP_STATUS" in
  301|302|303|307|308) ;;
  *) fail "auth /signup returned HTTP $SIGNUP_STATUS" ;;
esac
USER_ID="$(psql_exec -tA -v "email=$TEST_EMAIL" <<'SQL'
SELECT id::text FROM zeroship.users WHERE email = :'email'::citext;
SQL
)"
USER_ID="$(printf '%s' "$USER_ID" | tr -d '[:space:]')"
[ -n "$USER_ID" ] || fail "signup did not create test user"
psql_exec -q -v "email=$TEST_EMAIL" <<'SQL' >/dev/null
UPDATE zeroship.users SET email_verified_at = NOW() WHERE email = :'email'::citext;
SQL

psql_exec -q \
  -v "user_id=$USER_ID" \
  -v "session_id=$SESSION_ID" \
  <<'SQL' >/dev/null
INSERT INTO zeroship.idp_sessions
  (id, user_id, auth_method, amr, acr, credential_version, idle_expires_at, abs_expires_at)
SELECT
  :'session_id',
  id,
  'password',
  ARRAY['pwd']::text[],
  'urn:zeroship:pwd',
  credential_version,
  NOW() + INTERVAL '30 minutes',
  NOW() + INTERVAL '12 hours'
FROM zeroship.users
WHERE id = :'user_id';
SQL
pass "created password-backed user ${USER_ID} via /signup and seeded IdP session"
SEEDED_SESSION_OK="$(psql_exec -tAc \
  "SELECT COUNT(*) FROM zeroship.idp_sessions s JOIN zeroship.users u ON u.id = s.user_id WHERE s.id = '${SESSION_ID}' AND s.credential_version = u.credential_version AND s.revoked_at IS NULL AND s.idle_expires_at > NOW() AND s.abs_expires_at > NOW()" \
  | tr -d '[:space:]')"
[ "$SEEDED_SESSION_OK" = "1" ] || fail "seeded IdP session is not valid"
pass "seeded IdP session validates in DB"

step "Run zeroship login and approve the device code"
LOGIN_LOG="$WORKDIR/login.log"
ZEROSHIP_CONFIG_HOME="$CONFIG_HOME" "$BIN/zeroship" login --auth-url="$HYDRA_PUBLIC_URL" \
  >"$LOGIN_LOG" 2>&1 &
LOGIN_PID="$!"
PIDS+=("$LOGIN_PID")
USER_CODE="$(wait_for_login_code "$LOGIN_PID" "$LOGIN_LOG")" \
  || fail "zeroship login did not print a device user_code"
pass "zeroship login requested device user_code ${USER_CODE}"

DEVICE_HEADERS="$WORKDIR/device.headers"
curl -fsS -D "$DEVICE_HEADERS" -o "$WORKDIR/device.html" "$AUTH_URL/device"
CSRF="$(extract_csrf_cookie "$DEVICE_HEADERS")"
[ -n "$CSRF" ] || fail "GET /device did not set zsidp_csrf"

DEVICE_COOKIE_JAR="$WORKDIR/device-hydra.cookies"
DEVICE_STATUS="$(curl -sS -D "$WORKDIR/device-post.headers" -c "$DEVICE_COOKIE_JAR" -o "$WORKDIR/device-post.html" -w '%{http_code}' \
  -X POST "$AUTH_URL/device" \
  -H "Content-Type: application/x-www-form-urlencoded" \
  -H "Cookie: zsidp_session=${SESSION_ID}; zsidp_csrf=${CSRF}" \
  --data-urlencode "user_code=${USER_CODE}" \
  --data-urlencode "csrf=${CSRF}")"
DEVICE_LOCATION="$(tr -d '\r' < "$WORKDIR/device-post.headers" | awk 'BEGIN { IGNORECASE = 1 } /^location:/ { sub(/^location:[[:space:]]*/, ""); print; exit }')"
case "$DEVICE_STATUS" in
  301|302|303|307|308)
    [ "$DEVICE_LOCATION" != "/login" ] || fail "auth /device bounced to /login instead of accepting the signed-in session"
    [ -n "$DEVICE_LOCATION" ] || fail "auth /device did not return a completion redirect"
    curl -fsS -L -D "$WORKDIR/device-complete.headers" \
      -b "$DEVICE_COOKIE_JAR" -c "$DEVICE_COOKIE_JAR" \
      -o "$WORKDIR/device-complete.html" "$DEVICE_LOCATION" \
      || fail "following Hydra device completion redirect failed"
    ;;
  *) dump_file_tail "device post" "$WORKDIR/device-post.html"; fail "auth /device returned HTTP $DEVICE_STATUS" ;;
esac

LOGIN_ACTION="$(extract_form_action "$WORKDIR/device-complete.html")"
if printf '%s' "$LOGIN_ACTION" | grep -q '^/login?login_challenge='; then
  LOGIN_POST_URL="$(absolute_auth_url "$LOGIN_ACTION")"
  LOGIN_CSRF="$(extract_hidden_value csrf "$WORKDIR/device-complete.html")"
  [ -n "$LOGIN_CSRF" ] || fail "Hydra browser login page did not render a csrf token"
  BROWSER_LOGIN_STATUS="$(curl -sS -L -D "$WORKDIR/browser-login.headers" \
    -b "$DEVICE_COOKIE_JAR" -c "$DEVICE_COOKIE_JAR" \
    -o "$WORKDIR/browser-login.html" -w '%{http_code}' \
    -H "Content-Type: application/x-www-form-urlencoded" \
    "$LOGIN_POST_URL" \
    --data-urlencode "email=${TEST_EMAIL}" \
    --data-urlencode "password=${TEST_PASSWORD}" \
    --data-urlencode "csrf=${LOGIN_CSRF}")" \
    || fail "posting Hydra browser login form failed"
  case "$BROWSER_LOGIN_STATUS" in
    200|201|202|204|301|302|303|307|308) ;;
    *) fail "Hydra browser login/consent flow returned HTTP $BROWSER_LOGIN_STATUS" ;;
  esac
  if grep -q '<title>Sign in · zeroship</title>' "$WORKDIR/browser-login.html"; then
    fail "Hydra browser flow ended back at the login page"
  fi
else
  cp "$WORKDIR/device-complete.html" "$WORKDIR/browser-login.html"
fi
pass "auth /device accepted the user_code and Hydra browser login/consent completed"

if ! wait_for_login_exit "$LOGIN_PID"; then
  fail "zeroship login did not complete after device approval"
fi
CREDS="$CONFIG_HOME/zeroship/token.json"
[ -s "$CREDS" ] || fail "zeroship login did not write credentials"
ACCESS_TOKEN="$(jq -r '.access_token // empty' "$CREDS")"
[ "${#ACCESS_TOKEN}" -gt 20 ] || fail "credentials file does not contain a real access_token"
pass "zeroship login saved a real access_token"

INTROSPECT="$(curl -fsS -X POST "$HYDRA_ADMIN_URL/admin/oauth2/introspect" \
  -H "Content-Type: application/x-www-form-urlencoded" \
  --data-urlencode "token=${ACCESS_TOKEN}")"
echo "$INTROSPECT" | jq -e \
  --arg aud "$OAUTH_AUDIENCE" \
  '.active == true
    and ((.scope // "") | split(" ") | index("apps:deploy") != null)
    and ((.scope // "") | split(" ") | index("apps:write") != null)
    and ((.aud // []) | index($aud) != null)' >/dev/null \
  || fail "introspection did not show active apps:write/apps:deploy token with ${OAUTH_AUDIENCE} audience: ${INTROSPECT}"
pass "Hydra introspection sees active apps:write/apps:deploy token for ${OAUTH_AUDIENCE}"

step "Deploy with the logged-in OAuth bearer"
DEPLOY_LOG="$WORKDIR/deploy.log"
ZEROSHIP_CONFIG_HOME="$CONFIG_HOME" "$BIN/zeroship" deploy "$ZSHIP" \
  --app="$APP_NAME" \
  --control="$CONTROL_URL" \
  >"$DEPLOY_LOG" 2>&1 \
  || fail "zeroship deploy failed"
DEPLOY_HASH="$(sed -n 's/.*deploy_hash: //p' "$DEPLOY_LOG" | tail -1 | tr -d '[:space:]')"
[ -n "$DEPLOY_HASH" ] || fail "zeroship deploy did not print deploy_hash"
pass "zeroship deploy returned deploy_hash ${DEPLOY_HASH}"

sleep 4
API_KEY="$(psql_exec -tAc "SELECT api_key FROM zeroship.apps WHERE name = '${APP_NAME}'" | tr -d '[:space:]')"
[ -n "$API_KEY" ] || fail "could not read ${APP_NAME} api_key from fresh test DB"
INDEX="$(curl -sf "$GATE_URL/apps/${APP_NAME}/" -H "X-Api-Key: ${API_KEY}" 2>/dev/null || true)"
echo "$INDEX" | grep -qi '<!doctype html' \
  || fail "gateway did not serve ${APP_NAME} index.html"
pass "gateway served /apps/${APP_NAME}/ index.html"

printf '\n============================================\n'
printf '  auth deploy e2e: %d passed, %d failed\n' "$PASS" "$FAIL"
printf '============================================\n'
printf 'Route: B (direct binaries + Hydra container)\n'
printf 'Hydra client: zeroship-cli device grant registered, scopes=apps:write/apps:deploy, audience=%s\n' "$OAUTH_AUDIENCE"
printf 'Login: real access_token saved and introspected active (token redacted)\n'
printf 'Deploy: %s\n' "$DEPLOY_HASH"
printf 'Gateway: %s/apps/%s/ served index.html\n' "$GATE_URL" "$APP_NAME"

[ "$FAIL" -eq 0 ]
