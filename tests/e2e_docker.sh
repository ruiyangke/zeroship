#!/usr/bin/env bash
# Docker Compose E2E test - multi-node with distribution verification.
#
# Prerequisites: docker compose build
#
# Usage:
#   ./tests/e2e_docker.sh              # default: 3 workers
#   NUM_WORKERS=10 ./tests/e2e_docker.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=tests/lib/e2e_stack.sh
source "$ROOT/tests/lib/e2e_stack.sh"
# The base deployment intentionally gives control no host port. This harness
# needs localhost access for its HTTP assertions, so it adds an isolated,
# test-only loopback publication. Relative paths continue to resolve from the
# first compose file.
export COMPOSE_FILE="$ROOT/deploy/compose/docker-compose.yml:$ROOT/tests/e2e_docker.override.yml"

# PROJECT ISOLATION IS A SAFETY REQUIREMENT, NOT A CONVENIENCE.
#
# This script runs `docker compose down -v` twice, and `-v` DELETES VOLUMES.
# Without a project name, compose derives one from the compose file's directory,
# which is `compose` - and on 2026-08-12 `docker compose ls` showed that exact
# project already running with 11 containers, serving 127.0.0.1:9090 and :8000,
# owning the Postgres volume. Running this harness as written would have torn
# down the operator's live stack and destroyed its database, before printing a
# single assertion.
#
# Defaulting COMPOSE_PROJECT_NAME here means `down -v` can only ever reach a
# project this harness owns. Override it if you want, but do not unset it.
export COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-zs-docker-e2e}"

NUM_WORKERS=${NUM_WORKERS:-3}
GATE="http://localhost:8000"
CONTROL="http://localhost:9090"

PASS=0
FAIL=0

pass() { PASS=$((PASS + 1)); echo "  PASS: $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL: $1"; }

# http_status <method> <url> [curl args...] - echoes the HTTP status code, or
# 000 when the request never reached a server.
#
# THIS EXISTS BECAUSE THE OBVIOUS SPELLING CANNOT FAIL. The auth checks below
# used to read:
#     result=$(curl -sf ... || echo "rejected")
#     echo "$result" | grep -qi "missing\|unauthorized\|rejected"
# With `-f`, curl exits non-zero on ANY failure - a 401, yes, but equally a
# connection refused, a dead gateway, a DNS error - so the shell substitutes the
# literal word "rejected", and the grep then matches the string this script
# wrote itself. MEASURED 2026-08-12 against 127.0.0.1:1 with nothing listening:
# both auth assertions printed PASS having never contacted a server. Same class
# as #275/#297: an assertion satisfied by the failure mode it exists to exclude.
#
# Status codes are the honest observable, and 000 (curl's "no response") is
# distinguishable from every real rejection.
http_status() {
    local method="$1" url="$2"; shift 2
    # NO `|| echo "000"` HERE. curl already prints 000 on a failed request AND
    # exits non-zero, so the fallback APPENDED a second 000 and the caller saw
    # "000000" - which matches neither the 000 arm nor any real code, so it fell
    # through to the catch-all and the "gateway never answered" diagnostic I
    # wrote for exactly that case became unreachable. Caught 2026-08-12 in my own
    # green proof, one line after fixing the same class of defect below.
    # `|| true` swallows the status without adding output.
    curl -s -o /dev/null -w '%{http_code}' -X "$method" "$url" "$@" 2>/dev/null || true
}

echo "============================================"
echo "  Docker Compose E2E ($NUM_WORKERS workers)"
echo "============================================"
echo ""

# The stack now REQUIRES generated secrets; without them compose refuses to
# render. Idempotent, never rotates.
. "$(dirname "$0")/lib/dev_secrets.sh"
ensure_dev_secrets || exit 1

# --- Start cluster ---
echo "=== Starting cluster ==="
docker compose down -v > /dev/null 2>&1 || true
docker compose up -d --scale worker=$NUM_WORKERS 2>&1 | tail -3
sleep 5
docker compose restart gateway > /dev/null 2>&1
sleep 5

# --- Health ---
echo ""
echo "=== Test 1: Health ==="
curl -sf "$CONTROL/readyz" > /dev/null && pass "control" || fail "control"
curl -sf "$GATE/readyz" > /dev/null && pass "gateway" || fail "gateway"
RUNNING=$(docker compose ps worker --format json 2>/dev/null | jq -s 'length')
[ "$RUNNING" -eq "$NUM_WORKERS" ] && pass "$RUNNING workers running" || fail "expected $NUM_WORKERS workers, got $RUNNING"

# Creator APIs accept a platform OAuth bearer only, verified against the JWKS
# of the issuer control booted with. This stack runs the REAL OP, so the harness
# signs with the OP's OWN key (deploy/compose/secrets/auth-signing.pem, the file
# the auth container loads) under the OP's own issuer, rather than serving a
# JWKS of its own that control would not trust.
command -v node >/dev/null 2>&1 || { fail "node is required to mint a test bearer"; exit 1; }
[ -f "$E2E_JOSE_JS" ] || { fail "workspace jose is required to mint a test bearer"; exit 1; }
SECRETS="$ROOT/deploy/compose/secrets"
WORK="$(mktemp -d -t zs-docker-e2e-XXXXXX)"
trap 'rm -rf "$WORK"' EXIT
PG_CONTAINER="$(docker compose ps -q postgres)"
E2E_PG_DATABASE=zeroship
[ -n "$PG_CONTAINER" ] || { fail "compose postgres container is missing"; exit 1; }
# Read the issuer out of the RUNNING control container rather than re-deriving
# it from ZEROSHIP_DOMAIN here: compose resolves it from deploy/compose/.env,
# which this shell has not read, so a re-derivation would silently disagree
# with the string control compares `iss` against.
ZEROSHIP_AUTH_PLATFORM_ISSUER="$(docker compose exec -T control printenv ZEROSHIP_AUTH_PLATFORM_ISSUER 2>/dev/null | tr -d '\r\n')"
[ -n "$ZEROSHIP_AUTH_PLATFORM_ISSUER" ] || { fail "control container names no platform issuer"; exit 1; }
export ZEROSHIP_AUTH_PLATFORM_ISSUER
e2e_platform_op_up "$SECRETS/auth-signing.pem" "$WORK" || { fail "configure the compose issuer"; exit 1; }
mint_creator_bearer || { fail "mint compose admin bearer"; exit 1; }

# --- Create + Deploy 20 apps ---
echo ""
echo "=== Test 2: Create + Deploy 20 apps ==="
DEPLOYED=0
declare -A IDS
declare -A KEYS
for i in $(seq 1 20); do
    name="dkr-$(printf '%02d' $i)"
    result=$(curl -sf -X POST "$CONTROL/api/apps" \
        -H 'Content-Type: application/json' \
        -H "Authorization: Bearer $ADMIN_TOKEN" \
        -d "{\"name\":\"$name\"}")
    IDS[$name]=$(echo "$result" | jq -r '.id')
    # The create response no longer carries an api key, and nothing on the
    # request path ever validated one. Kept as an empty string so the header
    # sends below keep their shape.
    KEYS[$name]=""

    # Deploy via control container
    if [ -n "${IDS[$name]}" ] && [ "${IDS[$name]}" != "null" ] \
       && docker compose exec -T control sh -c "
        echo 'export function ping() { return \"I am $name\"; }' > /tmp/$name.js
        zeroship deploy /tmp/$name.js --app=${IDS[$name]} --control=http://localhost:9090 --token=$ADMIN_TOKEN 2>/dev/null
    " > /dev/null 2>&1; then
        DEPLOYED=$((DEPLOYED + 1))
    fi
done
# COUNTED, not announced. This used to be an unconditional `pass "20 apps
# created + deployed"` after a loop whose every failure was discarded (`curl
# -sf` with output thrown away, `docker compose exec ... > /dev/null 2>&1`), so
# a run in which all twenty creations AND all twenty deploys failed printed the
# same green line as a perfect run. Nothing recorded success, so there was
# nothing the pass could have been gated on.
[ "$DEPLOYED" -eq 20 ] && pass "20/20 apps created + deployed" \
                       || fail "only $DEPLOYED/20 apps created + deployed"
sleep 5

# --- Identity ---
echo ""
echo "=== Test 3: Identity (20 apps) ==="
ID_OK=0
for i in $(seq 1 20); do
    name="dkr-$(printf '%02d' $i)"
    result=$(curl -sf -X POST "$GATE/apps/$name/rpc" \
        -H 'Content-Type: application/json' \
        -H "X-Api-Key: ${KEYS[$name]}" \
        -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":1}' 2>/dev/null || echo "")
    returned=$(echo "$result" | jq -r '.result // empty')
    [ "$returned" = "I am $name" ] && ID_OK=$((ID_OK + 1))
done
[ $ID_OK -eq 20 ] && pass "20/20 identity correct" || fail "$ID_OK/20 correct"

# --- Distribution ---
echo ""
echo "=== Test 4: Worker distribution ==="
echo "  Apps per worker (from on-demand load logs):"
DIST=$(docker compose logs worker 2>&1 | grep "on-demand loaded" | awk -F'|' '{print $1}' | sed 's/ *$//' | sort | uniq -c | sort -rn)
echo "$DIST" | head -10 | while read count name; do
    printf "    %-15s %d apps\n" "$name" "$count"
done
# `echo "$DIST" | wc -l` yields 1 for an EMPTY DIST, because echo emits a
# newline regardless - so a run where no worker ever loaded an app printed
# "1 workers received traffic" and passed. Measured 2026-08-12: DIST="" gives
# USED=1, identical to DIST with one real line. Count non-blank lines instead,
# and gate the pass on there being any.
USED=$(printf '%s\n' "$DIST" | grep -c '[^[:space:]]' || true)
if [ "$USED" -gt 0 ]; then
    pass "$USED workers received traffic"
else
    fail "no worker logged an on-demand load - the dispatch never reached a worker"
fi

# --- Auth ---
echo ""
echo "=== Test 5: Auth ==="
code=$(http_status POST "$GATE/apps/dkr-01/rpc" -H 'Content-Type: application/json' -d '{}')
case "$code" in
    401|403) pass "no key rejected (HTTP $code)" ;;
    000)     fail "no key: gateway never answered (HTTP 000) - this used to PASS" ;;
    *)       fail "no key: expected 401/403, got HTTP $code" ;;
esac

code=$(http_status POST "$GATE/apps/nonexistent/rpc" -H 'Content-Type: application/json' -H 'X-Api-Key: any' -d '{}')
case "$code" in
    404) pass "unknown app rejected (HTTP 404)" ;;
    000) fail "unknown app: gateway never answered (HTTP 000) - this used to PASS" ;;
    *)   fail "unknown app: expected 404, got HTTP $code" ;;
esac

# --- Cleanup ---
echo ""
echo "=== Cleanup ==="
docker compose down -v > /dev/null 2>&1
pass "cluster stopped"

echo ""
echo "============================================"
echo "  Results: $PASS passed, $FAIL failed"
echo "============================================"
[ $FAIL -eq 0 ] && exit 0 || exit 1
