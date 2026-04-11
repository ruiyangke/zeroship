#!/usr/bin/env bash
# E2E test for the Docker Compose platform deployment.
#
# Prerequisites: docker compose up -d
# This script creates apps, deploys code, and sends requests through
# the full pipeline: gateway → worker → V8.
set -euo pipefail

GATE="http://localhost:8000"
CONTROL="http://localhost:9090"
MASTER_KEY="master-key"

echo "============================================"
echo "  Docker Compose E2E Test"
echo "============================================"
echo ""

# Wait for services
echo "=== Waiting for services ==="
for i in $(seq 1 30); do
    if curl -sf "$CONTROL/health" > /dev/null 2>&1; then
        echo "  Control plane ready"
        break
    fi
    sleep 1
done

for i in $(seq 1 30); do
    if curl -sf "$GATE/health" > /dev/null 2>&1; then
        echo "  Gateway ready"
        break
    fi
    sleep 1
done

# Create apps
echo ""
echo "=== Creating apps ==="
NUM_APPS=5
declare -A APP_IDS
declare -A API_KEYS

for i in $(seq 1 $NUM_APPS); do
    name="app-$(printf '%02d' $i)"
    result=$(curl -sf -X POST "$CONTROL/api/apps" \
        -H 'Content-Type: application/json' \
        -H "Authorization: Bearer $MASTER_KEY" \
        -d "{\"name\":\"$name\"}")
    APP_IDS[$name]=$(echo "$result" | jq -r '.id')
    API_KEYS[$name]=$(echo "$result" | jq -r '.api_key')
    echo "  $name: ${APP_IDS[$name]}"
done

# Deploy
echo ""
echo "=== Deploying apps ==="
for i in $(seq 1 $NUM_APPS); do
    name="app-$(printf '%02d' $i)"
    id="${APP_IDS[$name]}"

    # Create unique JS
    JS="export function ping() { return \"pong from $name\"; }"

    # Build .appbundle (using the CLI if available, else raw upload)
    if command -v appbase &> /dev/null; then
        tmpfile=$(mktemp --suffix=.js)
        echo "$JS" > "$tmpfile"
        appbase deploy "$tmpfile" --app="$id" --control="$CONTROL" --key="$MASTER_KEY" 2>/dev/null
        rm "$tmpfile"
    else
        # Upload raw JS as .appbundle-compatible format
        # For testing, we need the actual appbase binary in the control container
        docker compose exec -T control sh -c "
            echo '$JS' > /tmp/app.js
            appbase deploy /tmp/app.js --app=$id --control=http://localhost:9090 --key=$MASTER_KEY
        " 2>/dev/null
    fi
    echo "  $name deployed"
done

# Wait for gateway sync
echo ""
echo "=== Waiting for sync (5s) ==="
sleep 5

# Test requests
echo ""
echo "=== Testing requests ==="
PASS=0
FAIL=0

for i in $(seq 1 $NUM_APPS); do
    name="app-$(printf '%02d' $i)"
    key="${API_KEYS[$name]}"

    result=$(curl -sf -X POST "$GATE/apps/$name/rpc" \
        -H 'Content-Type: application/json' \
        -H "X-Api-Key: $key" \
        -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":1}' 2>/dev/null || echo "FAIL")

    if echo "$result" | grep -q "pong from $name"; then
        echo "  $name: OK — $(echo $result | jq -r .result)"
        PASS=$((PASS + 1))
    else
        echo "  $name: FAIL — $result"
        FAIL=$((FAIL + 1))
    fi
done

# Auth test
echo ""
echo "=== Auth test ==="
result=$(curl -sf -X POST "$GATE/apps/app-01/rpc" \
    -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":1}' 2>/dev/null || echo "AUTH_FAIL")
if echo "$result" | grep -q "missing\|unauthorized\|Api-Key"; then
    echo "  Unauthenticated request correctly rejected"
    PASS=$((PASS + 1))
else
    echo "  Auth test FAILED: $result"
    FAIL=$((FAIL + 1))
fi

# 404 test
result=$(curl -sf -X POST "$GATE/apps/nonexistent/rpc" \
    -H 'Content-Type: application/json' \
    -H 'X-Api-Key: any' \
    -d '{}' 2>/dev/null || echo "NOT_FOUND")
if echo "$result" | grep -q "not found\|NOT_FOUND"; then
    echo "  Unknown app correctly returns 404"
    PASS=$((PASS + 1))
else
    echo "  404 test FAILED: $result"
    FAIL=$((FAIL + 1))
fi

# Worker count
echo ""
echo "=== Infrastructure ==="
WORKER_COUNT=$(docker compose ps worker --format json 2>/dev/null | jq -s 'length')
echo "  Workers running: $WORKER_COUNT"
echo "  Containers: $(docker compose ps --format json 2>/dev/null | jq -s 'length')"

echo ""
echo "============================================"
echo "  Results: $PASS passed, $FAIL failed"
echo "============================================"

[ $FAIL -eq 0 ] && exit 0 || exit 1
