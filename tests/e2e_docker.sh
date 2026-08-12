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
MASTER_KEY="master-key"

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
curl -sf "$CONTROL/health" > /dev/null && pass "control" || fail "control"
curl -sf "$GATE/health" > /dev/null && pass "gateway" || fail "gateway"
RUNNING=$(docker compose ps worker --format json 2>/dev/null | jq -s 'length')
[ "$RUNNING" -eq "$NUM_WORKERS" ] && pass "$RUNNING workers running" || fail "expected $NUM_WORKERS workers, got $RUNNING"

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
        -H "Authorization: Bearer $MASTER_KEY" \
        -d "{\"name\":\"$name\"}")
    IDS[$name]=$(echo "$result" | jq -r '.id')
    KEYS[$name]=$(echo "$result" | jq -r '.api_key')

    # Deploy via control container
    if [ -n "${IDS[$name]}" ] && [ "${IDS[$name]}" != "null" ] \
       && docker compose exec -T control sh -c "
        echo 'export function ping() { return \"I am $name\"; }' > /tmp/$name.js
        zeroship deploy /tmp/$name.js --app=${IDS[$name]} --control=http://localhost:9090 --key=$MASTER_KEY 2>/dev/null
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
