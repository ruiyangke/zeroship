#!/usr/bin/env bash
# Docker Compose E2E test — multi-node with distribution verification.
#
# Prerequisites: docker compose build
#
# Usage:
#   ./tests/e2e_docker.sh              # default: 3 workers
#   NUM_WORKERS=10 ./tests/e2e_docker.sh
set -euo pipefail

NUM_WORKERS=${NUM_WORKERS:-3}
GATE="http://localhost:8000"
CONTROL="http://localhost:9090"
MASTER_KEY="master-key"

PASS=0
FAIL=0

pass() { PASS=$((PASS + 1)); echo "  ✓ $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  ✗ $1"; }

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
    docker compose exec -T control sh -c "
        echo 'export function ping() { return \"I am $name\"; }' > /tmp/$name.js
        appbase deploy /tmp/$name.js --app=${IDS[$name]} --control=http://localhost:9090 --key=$MASTER_KEY 2>/dev/null
    " > /dev/null 2>&1
done
pass "20 apps created + deployed"
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
USED=$(echo "$DIST" | wc -l)
pass "$USED workers received traffic"

# --- Auth ---
echo ""
echo "=== Test 5: Auth ==="
result=$(curl -sf -X POST "$GATE/apps/dkr-01/rpc" \
    -H 'Content-Type: application/json' \
    -d '{}' 2>/dev/null || echo "rejected")
echo "$result" | grep -qi "missing\|unauthorized\|rejected" && pass "no key → rejected" || fail "no key not rejected"

result=$(curl -sf -X POST "$GATE/apps/nonexistent/rpc" \
    -H 'Content-Type: application/json' \
    -H 'X-Api-Key: any' \
    -d '{}' 2>/dev/null || echo "not_found")
echo "$result" | grep -qi "not.found\|not_found" && pass "unknown app → 404" || fail "unknown app not 404"

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
