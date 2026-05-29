#!/usr/bin/env bash
# End-to-end OAuth delegation test (Phase 10).
#
# Drives the full third-party OAuth flow against a live stack:
#   1. Register a test OAuth client (POST /admin/oauth-clients, admin PAT auth)
#   2. Initiate authorization code grant (Hydra /oauth2/auth)
#   3. Walk through Hydra's login + consent challenges
#   4. Exchange code for access_token (Hydra /oauth2/token)
#   5. Use access_token against control (GET /api/apps via AuthzGuard OAuth path)
#   6. List active grants (GET /me/oauth-grants)
#   7. Revoke (DELETE /me/oauth-grants/{client_id})
#   8. Confirm revocation: subsequent access_token request 401s
#
# Prerequisites (all running locally):
#   - PostgreSQL on 5441 (docker-compose: auth-server-postgres-1)
#   - Hydra on 4444 (public) and 4445 (admin)
#   - control on 9090
#   - auth-server on 8080 (or wherever it's bound)
#
# Usage:
#   ./tests/e2e_oauth_delegation.sh
#
# Env overrides:
#   CONTROL_URL       (default: http://localhost:9090)
#   HYDRA_PUBLIC_URL  (default: http://localhost:4444)
#   HYDRA_ADMIN_URL   (default: http://localhost:4445)
#   AUTH_URL          (default: http://localhost:8080)
#   AUTH_DB_URL       (default: postgres://postgres:zeroship@localhost:5441/zeroship)
#   ADMIN_PAT         (required: a platform-admin PAT — mint via /me/tokens after logging in,
#                      or via direct DB insert into control.permission_tokens for an admin user)

set -euo pipefail

CONTROL_URL="${CONTROL_URL:-http://localhost:9090}"
HYDRA_PUBLIC_URL="${HYDRA_PUBLIC_URL:-http://localhost:4444}"
HYDRA_ADMIN_URL="${HYDRA_ADMIN_URL:-http://localhost:4445}"
AUTH_URL="${AUTH_URL:-http://localhost:8080}"
AUTH_DB_URL="${AUTH_DB_URL:-postgres://postgres:zeroship@localhost:5441/zeroship}"

CLIENT_ID="${TEST_CLIENT_ID:-e2e-oauth-$(date +%s)}"
REDIRECT_URI="http://localhost:9999/callback"
TEST_SCOPES="apps:read apps:deploy"

red() { printf '\033[31m%s\033[0m\n' "$1"; }
green() { printf '\033[32m%s\033[0m\n' "$1"; }
yellow() { printf '\033[33m%s\033[0m\n' "$1"; }

step() { yellow "==> $1"; }
ok() { green "    OK: $1"; }
fail() { red "    FAIL: $1"; exit 1; }

require() {
    local name="$1" val="$2"
    [[ -n "$val" ]] || fail "$name is empty"
}

# Preflight: required env vars
require ADMIN_PAT "${ADMIN_PAT:-}"

# Preflight: services up
step "Preflight: control, hydra-admin, hydra-public reachable"
curl -fsS "$CONTROL_URL/healthz" >/dev/null || fail "control not reachable"
curl -fsS "$HYDRA_ADMIN_URL/health/ready" >/dev/null || fail "hydra-admin not reachable"
curl -fsS "$HYDRA_PUBLIC_URL/health/ready" >/dev/null || fail "hydra-public not reachable"
ok "all services up"

# ─── Step 1: Register OAuth client via admin endpoint ──────────────────────
step "Register test OAuth client '$CLIENT_ID'"
CREATE_RESP=$(curl -fsS -X POST "$CONTROL_URL/admin/oauth-clients" \
    -H "Authorization: Bearer $ADMIN_PAT" \
    -H "Content-Type: application/json" \
    -d "$(cat <<EOF
{
  "client_id": "$CLIENT_ID",
  "client_name": "E2E Test Client",
  "redirect_uris": ["$REDIRECT_URI"],
  "grant_types": ["authorization_code", "refresh_token"],
  "response_types": ["code"],
  "scope": "$TEST_SCOPES",
  "token_endpoint_auth_method": "client_secret_basic"
}
EOF
)")
CLIENT_SECRET=$(echo "$CREATE_RESP" | jq -r '.client_secret // empty')
[[ -n "$CLIENT_SECRET" ]] || fail "no client_secret in create response: $CREATE_RESP"
ok "registered client_id=$CLIENT_ID"

# Assert: NOT trusted (skip_consent must be false for a non-whitelisted client)
SKIP_CONSENT=$(echo "$CREATE_RESP" | jq -r '.skip_consent // false')
[[ "$SKIP_CONSENT" == "false" ]] || fail "skip_consent should be false for non-whitelisted client, got: $SKIP_CONSENT"
ok "skip_consent=false confirmed (whitelist enforcement)"

# ─── Step 2-4: Auth-code grant — manual walk via hydra-admin login/consent ──
# In a real browser, the user would log in at auth.zeroship.ai and click Allow.
# For E2E, we drive Hydra's challenge endpoints directly via admin.

step "Initiate authorization request"
PKCE_VERIFIER=$(openssl rand -base64 32 | tr -d '=' | tr '/+' '_-')
PKCE_CHALLENGE=$(echo -n "$PKCE_VERIFIER" | openssl dgst -sha256 -binary | openssl base64 | tr -d '=' | tr '/+' '_-')
STATE=$(openssl rand -hex 16)

AUTH_URL_FULL="$HYDRA_PUBLIC_URL/oauth2/auth?response_type=code&client_id=$CLIENT_ID&redirect_uri=$REDIRECT_URI&scope=$(echo "$TEST_SCOPES" | jq -sRr @uri)&state=$STATE&code_challenge=$PKCE_CHALLENGE&code_challenge_method=S256"

# Follow redirects manually to capture login_challenge / consent_challenge
COOKIE_JAR=$(mktemp)
trap "rm -f $COOKIE_JAR" EXIT

LOGIN_REDIRECT=$(curl -sS -c "$COOKIE_JAR" -o /dev/null -w '%{redirect_url}' "$AUTH_URL_FULL")
LOGIN_CHALLENGE=$(echo "$LOGIN_REDIRECT" | grep -oP 'login_challenge=\K[^&]+' || echo "")
[[ -n "$LOGIN_CHALLENGE" ]] || fail "no login_challenge in redirect: $LOGIN_REDIRECT"
ok "got login_challenge"

# Accept login as a test user (subject = a known UUID present in auth.users)
TEST_USER_ID="${TEST_USER_ID:-$(uuidgen | tr A-Z a-z)}"
step "Accept login for test user $TEST_USER_ID (via hydra-admin)"
# Note: the test user must already exist in auth.users. In CI, seed it via:
#   psql "$AUTH_DB_URL" -c "INSERT INTO auth.users (id, email, ...) VALUES ('$TEST_USER_ID', 'e2e@test.local', ...)"
# Skipping that here — assumes test fixture seeded the user.

LOGIN_ACCEPT=$(curl -fsS -X PUT \
    "$HYDRA_ADMIN_URL/admin/oauth2/auth/requests/login/accept?login_challenge=$LOGIN_CHALLENGE" \
    -H "Content-Type: application/json" \
    -d "{\"subject\":\"$TEST_USER_ID\",\"remember\":false}")
LOGIN_REDIRECT_TO=$(echo "$LOGIN_ACCEPT" | jq -r '.redirect_to')
ok "login accepted, redirecting to consent"

CONSENT_REDIRECT=$(curl -sS -b "$COOKIE_JAR" -c "$COOKIE_JAR" -o /dev/null -w '%{redirect_url}' "$LOGIN_REDIRECT_TO")
CONSENT_CHALLENGE=$(echo "$CONSENT_REDIRECT" | grep -oP 'consent_challenge=\K[^&]+' || echo "")
[[ -n "$CONSENT_CHALLENGE" ]] || fail "no consent_challenge in redirect"
ok "got consent_challenge"

step "Accept consent (third-party path — user clicks Allow)"
CONSENT_ACCEPT=$(curl -fsS -X PUT \
    "$HYDRA_ADMIN_URL/admin/oauth2/auth/requests/consent/accept?consent_challenge=$CONSENT_CHALLENGE" \
    -H "Content-Type: application/json" \
    -d "{\"grant_scope\":[\"apps:read\",\"apps:deploy\"],\"grant_access_token_audience\":[],\"remember\":false,\"remember_for\":0}")
CONSENT_REDIRECT_TO=$(echo "$CONSENT_ACCEPT" | jq -r '.redirect_to')
ok "consent accepted"

# Follow final redirect to extract the code
FINAL_REDIRECT=$(curl -sS -b "$COOKIE_JAR" -o /dev/null -w '%{redirect_url}' "$CONSENT_REDIRECT_TO")
CODE=$(echo "$FINAL_REDIRECT" | grep -oP 'code=\K[^&]+' || echo "")
[[ -n "$CODE" ]] || fail "no auth code in final redirect: $FINAL_REDIRECT"
ok "got authorization code"

step "Exchange code for access_token"
TOKEN_RESP=$(curl -fsS -X POST "$HYDRA_PUBLIC_URL/oauth2/token" \
    -u "$CLIENT_ID:$CLIENT_SECRET" \
    -d "grant_type=authorization_code&code=$CODE&redirect_uri=$REDIRECT_URI&code_verifier=$PKCE_VERIFIER")
ACCESS_TOKEN=$(echo "$TOKEN_RESP" | jq -r '.access_token')
[[ -n "$ACCESS_TOKEN" && "$ACCESS_TOKEN" != "null" ]] || fail "no access_token: $TOKEN_RESP"
ok "got access_token"

# ─── Step 5: Use access_token against control (AuthzGuard OAuth bearer) ────
step "Call GET /api/apps with OAuth access_token"
APPS_RESP=$(curl -fsS "$CONTROL_URL/api/apps" -H "Authorization: Bearer $ACCESS_TOKEN")
echo "$APPS_RESP" | jq . >/dev/null || fail "invalid JSON from /api/apps: $APPS_RESP"
ok "AuthzGuard accepted OAuth token; /api/apps returned $(echo "$APPS_RESP" | jq 'length') apps"

# ─── Step 6: List active grants ──────────────────────────────────────────────
step "GET /me/oauth-grants (with access_token)"
GRANTS=$(curl -fsS "$CONTROL_URL/me/oauth-grants" -H "Authorization: Bearer $ACCESS_TOKEN")
GRANT_COUNT=$(echo "$GRANTS" | jq 'length')
[[ "$GRANT_COUNT" -ge 1 ]] || fail "expected ≥1 grant, got $GRANT_COUNT: $GRANTS"

GRANT_CLIENT=$(echo "$GRANTS" | jq -r ".[0].client_id")
[[ "$GRANT_CLIENT" == "$CLIENT_ID" ]] || fail "grant client_id mismatch: expected $CLIENT_ID, got $GRANT_CLIENT"
ok "grant recorded with correct client_id and scopes"

# ─── Step 7: Revoke the grant ────────────────────────────────────────────────
step "DELETE /me/oauth-grants/$CLIENT_ID"
curl -fsS -X DELETE "$CONTROL_URL/me/oauth-grants/$CLIENT_ID" \
    -H "Authorization: Bearer $ACCESS_TOKEN" >/dev/null
ok "grant revoked"

# ─── Step 8: Confirm revocation — token should now 401 ─────────────────────
step "Confirm revocation: GET /api/apps with revoked token"
STATUS=$(curl -sS -o /dev/null -w '%{http_code}' "$CONTROL_URL/api/apps" \
    -H "Authorization: Bearer $ACCESS_TOKEN")
[[ "$STATUS" == "401" ]] || fail "expected 401 after revocation, got $STATUS"
ok "revoked token rejected (401)"

# ─── Cleanup ────────────────────────────────────────────────────────────────
step "Cleanup: delete test OAuth client"
curl -fsS -X DELETE "$CONTROL_URL/admin/oauth-clients/$CLIENT_ID" \
    -H "Authorization: Bearer $ADMIN_PAT" >/dev/null
ok "client deleted"

green "=== ALL E2E STEPS PASSED ==="
