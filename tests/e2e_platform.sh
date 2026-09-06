#!/usr/bin/env bash
# End-to-end platform test suite.
#
# Tests:
#   1. Health checks (all components)
#   2. App lifecycle (create, deploy, archive, unarchive)
#   3. Identity verification (each app returns its own response)
#   4. Routing consistency (CHWBL: same app → same worker)
#   5. Isolation (app-01 state doesn't leak into app-02)
#   6. Auth enforcement (gateway rpc auth gate)
#   7. Worker on-demand loading (cold start)
#   8. Hot deploy (update code while serving)
#   9. Deploy edge cases (content-type / size cap / auth, pre-body)
#
# THREE workers on purpose: tests 4 and 5 are about CHWBL routing
# consistency and cross-worker isolation, which a single worker cannot
# show. That is why this file does its own bring-up instead of calling
# `stack_up` from tests/lib/e2e_stack.sh; it borrows the workspace, the
# ephemeral-Postgres + migration bring-up and the admin-bearer mint from there.
#
# Prerequisites:
#   - cargo build --release
#   - pnpm install && pnpm build
#   - pnpm install (jose, used by the harness OP to sign the admin bearer)
#   - docker (an EPHEMERAL Postgres is started and removed by this script)
#
# Usage:
#   ./tests/e2e_platform.sh
#
# Env overrides:
#   PG_PORT / PG_CONTAINER   — ephemeral Postgres port + container name
#
# NOTE ON THE DATABASE. This harness used to point at the long-lived
# `compose-postgres-1` and open with
# `DROP TABLE IF EXISTS usage_history, usage, apps CASCADE`. That is why
# that database has no `zeroship.apps` table today (measured 2026-08-09:
# 101 tables in schema `zeroship`, `apps` absent) — the harness deleted
# it, and the platform migration ledger considers the migration that
# created it already applied, so nothing puts it back. It now starts its
# own Postgres, applies db/migrations-ts from scratch, and removes the
# container on exit. Nothing shared is mutated.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release"

ZEROSHIP_CONTROL_PORT=9090
WORKER_PORTS=(8080 8081 8082)
ZEROSHIP_GATEWAY_PORT=8000
PG_PORT="${PG_PORT:-5456}"
PG_CONTAINER="${PG_CONTAINER:-zs-e2e-platform-pg}"
ZEROSHIP_CONTROL_KEY="test-ck"

PASS=0
FAIL=0
PIDS=()

pass() { PASS=$((PASS + 1)); echo "  ✓ $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  ✗ $1"; }

# The stack lib is SOURCED for stack_workspace / stack_pg_up /
# mint_creator_bearer. Sourcing (rather than copying mint_creator_bearer here) is
# deliberate: the creator credential is a three-part contract — users row, a
# JWKS the named issuer actually serves, and the at+jwt header/claim shape
# control verifies — and a copy of it in this file
# would drift from the control plane the next time any of the four moves.
# stack_workspace is what stands the issuer up. stack_up() is defined but
# never called; its port defaults use `:=` so the ports set above win.
# shellcheck source=tests/lib/e2e_stack.sh
. "$ROOT/tests/lib/e2e_stack.sh"

REACHED_SUMMARY=0
cleanup() {
    if [ ${#PIDS[@]} -gt 0 ]; then
        for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
    fi
    wait 2>/dev/null || true
    docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
    # Keep $WORK (control/worker/gateway logs + the migration log) whenever the
    # run did not finish clean. An aborted bring-up is exactly when someone
    # needs control.log, and deleting it is how the previous refuse-to-start
    # causes stayed invisible for four cycles.
    if [ -n "${WORK:-}" ]; then
        if [ "$REACHED_SUMMARY" = 1 ] && [ "$FAIL" -eq 0 ]; then
            rm -rf "$WORK"
        else
            echo "  logs kept: $WORK"
        fi
    fi
    return 0
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# HTTP helper
#
# Every request in this file goes through `http`/`http_ok`. The previous
# version used `VAR=$(curl -sf ...)` at nineteen sites: under
# `set -euo pipefail`, `-f` throws the body away and the non-zero exit
# propagates out of the command substitution, so ANY failing request ended
# the whole run with no output at all — indistinguishable from a pass, since
# the summary never printed either. `http` never aborts the run and always
# has the status and body available to report.
#
#   http METHOD URL [curl args...]   → sets HTTP_STATUS/HTTP_BODY, returns 0
#                                      unless curl itself failed (status 000)
#   http_ok METHOD URL [curl args...]→ as above, plus non-2xx returns 1 after
#                                      printing status + body
# ---------------------------------------------------------------------------
HTTP_STATUS=""
HTTP_BODY=""

http() {
    local method="$1" url="$2"; shift 2
    local raw rc
    set +e
    raw="$(curl -sS -m 180 -X "$method" -w $'\n%{http_code}' "$@" "$url" 2>&1)"
    rc=$?
    set -e
    if [ $rc -ne 0 ]; then
        HTTP_STATUS="000"
        HTTP_BODY="$raw"
        printf '    curl transport failure (exit %s): %s %s\n      %s\n' \
            "$rc" "$method" "$url" "$(printf '%s' "$raw" | tr '\n' ' ' | cut -c1-300)" >&2
        return 1
    fi
    HTTP_STATUS="${raw##*$'\n'}"
    HTTP_BODY="${raw%$'\n'*}"
    return 0
}

http_ok() {
    http "$@" || return 1
    case "$HTTP_STATUS" in
        2*) return 0 ;;
    esac
    printf '    HTTP %s: %s %s\n      body: %s\n' \
        "$HTTP_STATUS" "$1" "$2" "$(printf '%s' "$HTTP_BODY" | tr '\n' ' ' | cut -c1-400)" >&2
    return 1
}

jget() { printf '%s' "$1" | jq -r "$2 // empty" 2>/dev/null || true; }

# ---------------------------------------------------------------------------
# Fixture builder
#
# build_zship <js_file> <out_zship_path>
#
# Pack a single-module worker into a `.zship` archive (tar.zst) the control
# plane accepts at `POST /api/apps/{id}/deploy` with
# `Content-Type: application/x-zship`. Manifest is the first tar entry, the JS
# payload lives at `blobs/<sha256>`.
#
# Manifest shape is v1 `resources` (`crates/zeroship-bundle/src/manifest.rs`;
# `validate()` rejects any version but 1). The previous version of this file
# emitted `{"version":2,"rules":[...]}` with `POST /_rpc/* → rpc`, a shape
# that predates RPC v1 (fe571dc03, 2026-04-30) — both the version and the
# routing model. Optional 3rd arg overrides the resources object.
build_zship() {
    local js_file="$1" out_path="$2" resources="${3:-}"
    if [ -z "$resources" ]; then
        resources='{"/[...rest]":{"auth":"anonymous","publicly_accessible":true}}'
    fi

    local stage; stage=$(mktemp -d -t zeroship-e2e-zship-XXXXXX)
    mkdir -p "$stage/blobs"

    # SHA-256 the raw JS bytes — this hash is the blob's filename inside
    # the tar AND the value referenced in `manifest.worker.modules`.
    local hash
    hash=$(sha256sum "$js_file" | awk '{print $1}')
    cp "$js_file" "$stage/blobs/$hash"

    local now
    now=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
    cat > "$stage/manifest.json" <<EOF
{"version":1,"resources":$resources,"assets":{},"runtime_assets":{},"asset_version":0,"sourcemaps":{},"worker":{"entry":"index.js","modules":{"index.js":"$hash"}},"metadata":{"compiler":"e2e-test-fixture","built_at":"$now"}}
EOF

    # Tar manifest.json first, then blobs/<hash>. Listing files explicitly
    # avoids a directory entry and pins the order.
    (cd "$stage" && tar --format=ustar -cf - manifest.json "blobs/$hash") \
        | zstd -q -f -o "$out_path"
    rm -rf "$stage"
}

# create_app <name> → echoes the app id, or returns 1 having reported why.
create_app() {
    local name="$1"
    if ! http_ok POST "http://localhost:$ZEROSHIP_CONTROL_PORT/api/apps" \
        -H 'Content-Type: application/json' \
        -H "Authorization: Bearer $ADMIN_TOKEN" \
        -d "{\"name\":\"$name\"}"; then
        return 1
    fi
    local id; id="$(jget "$HTTP_BODY" '.id')"
    if [ -z "$id" ]; then
        echo "    create_app($name): no .id in $HTTP_BODY" >&2
        return 1
    fi
    printf '%s' "$id"
}

# deploy_js <app_id> <js_source> [resources_json] → 0 on a deploy_hash reply
deploy_js() {
    local app_id="$1" src="$2" resources="${3:-}"
    local tmpf tmpz out rc
    tmpf=$(mktemp --suffix=.js); tmpz=$(mktemp --suffix=.zship)
    printf '%s\n' "$src" > "$tmpf"
    if [ -n "$resources" ]; then build_zship "$tmpf" "$tmpz" "$resources"; else build_zship "$tmpf" "$tmpz"; fi
    set +e
    out="$("$BIN/zeroship" deploy "$tmpz" --app="$app_id" \
        --control="http://localhost:$ZEROSHIP_CONTROL_PORT" --token="$ADMIN_TOKEN" 2>&1)"
    rc=$?
    set -e
    rm -f "$tmpf" "$tmpz"
    if [ $rc -ne 0 ] || ! printf '%s' "$out" | grep -q "deploy_hash"; then
        echo "    deploy($app_id) failed (exit $rc): $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-400)" >&2
        return 1
    fi
    return 0
}

echo "============================================"
echo "  zeroship E2E Platform Test"
echo "============================================"
echo ""

# --- Setup ---
echo "=== Setup ==="
stack_preflight || { echo "  ✗ preflight failed"; exit 2; }
for port in $ZEROSHIP_CONTROL_PORT "${WORKER_PORTS[@]}" $ZEROSHIP_GATEWAY_PORT; do
    lsof -ti :"$port" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
done

stack_workspace || { echo "  ✗ stack_workspace failed"; exit 2; }
stack_pg_up || { echo "  ✗ ephemeral Postgres bring-up failed"; exit 2; }

# Start control. ZEROSHIP_AUTH_PLATFORM_ISSUER is load-bearing and comes from
# stack_workspace: control reads it ONCE at boot and fetches that issuer's JWKS
# to verify the admin bearer. Name an issuer nothing serves and every
# `/api/apps` call answers 401 {"error":"platform_token_verification_failed"},
# which is exactly how this harness died before, silently, inside `$(curl -sf)`.
"$BIN/zeroship-control" --port $ZEROSHIP_CONTROL_PORT --blob-store "$WORK/blobs" \
    > "$WORK/control.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 1 30); do curl -sf "http://localhost:$ZEROSHIP_CONTROL_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done

# Start 3 separate workers (so we can verify routing)
WORKER_URL_LIST=""
for port in "${WORKER_PORTS[@]}"; do
    "$BIN/zeroship-worker" --port "$port" --threads 2 --control-url "http://localhost:$ZEROSHIP_CONTROL_PORT" \
 --blob-store "$WORK/blobs" \
        --poll-interval 2 > "$WORK/worker-$port.log" 2>&1 &
    PIDS+=($!)
    [ -n "$WORKER_URL_LIST" ] && WORKER_URL_LIST="$WORKER_URL_LIST,"
    WORKER_URL_LIST="${WORKER_URL_LIST}http://localhost:${port}"
done

# Start gateway. cd54028e7 made it refuse to start without a broker master
# secret; $WORK/gate-secret comes from stack_workspace.
"$BIN/zeroship-gate" --port $ZEROSHIP_GATEWAY_PORT --control-url "http://localhost:$ZEROSHIP_CONTROL_PORT" \
 --worker-urls "$WORKER_URL_LIST" --poll-interval 2 \
 --blob-store "$WORK/blobs" --blob-cache-disk-root "$WORK/blob-cache" \
    --signing-key-file "$WORK/signing-key.pem" \
    --broker-secret-file "$WORK/gate-secret" \
    > "$WORK/gate.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 1 30); do curl -sf "http://localhost:$ZEROSHIP_GATEWAY_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done
echo "  control=$ZEROSHIP_CONTROL_PORT workers=${WORKER_PORTS[*]} gateway=$ZEROSHIP_GATEWAY_PORT pg=$PG_PORT"
echo "  logs in $WORK"

# ---------------------------------------------------------------------------
# Test 1: Health checks
# ---------------------------------------------------------------------------
echo ""
echo "=== Test 1: Health checks ==="
if http_ok GET "http://localhost:$ZEROSHIP_CONTROL_PORT/readyz"; then pass "control healthy"
else fail "control unhealthy"; tail -15 "$WORK/control.log" | sed 's/^/      /'; fi
for port in "${WORKER_PORTS[@]}"; do
    if http_ok GET "http://localhost:$port/readyz"; then pass "worker:$port healthy"
    else fail "worker:$port unhealthy"; tail -15 "$WORK/worker-$port.log" | sed 's/^/      /'; fi
done
if http_ok GET "http://localhost:$ZEROSHIP_GATEWAY_PORT/readyz"; then pass "gateway healthy"
else fail "gateway unhealthy"; tail -15 "$WORK/gate.log" | sed 's/^/      /'; fi

# The whole suite needs an admin bearer. Without it nothing below can run, so
# this is the one place that aborts.
echo ""
echo "=== Setup: admin bearer ==="
mint_creator_bearer || { echo "  ✗ cannot mint admin bearer — aborting"; echo "  Results: $PASS passed, $((FAIL + 1)) failed"; exit 1; }

# ---------------------------------------------------------------------------
# Test 2: App lifecycle
# ---------------------------------------------------------------------------
echo ""
echo "=== Test 2: App lifecycle ==="

APP_ID="$(create_app lifecycle-test || true)"
if [ -n "$APP_ID" ]; then pass "create app ($APP_ID)"; else fail "create app"; fi

if [ -n "$APP_ID" ]; then
    if deploy_js "$APP_ID" 'export default { fetch() { return new Response("lifecycle-ok"); } };'; then
        pass "deploy"
    else
        fail "deploy"
    fi

    sleep 3
    if http_ok GET "http://localhost:$ZEROSHIP_CONTROL_PORT/internal/versions" -H "Authorization: Bearer $ZEROSHIP_CONTROL_KEY"; then
        if printf '%s' "$HTTP_BODY" | jq -e ".[\"$APP_ID\"]" > /dev/null 2>&1; then
            pass "version in internal API"
        else
            fail "version missing from /internal/versions: $(printf '%s' "$HTTP_BODY" | cut -c1-200)"
        fi
    else
        fail "/internal/versions unreachable"
    fi

    if http_ok PUT "http://localhost:$ZEROSHIP_CONTROL_PORT/api/apps/$APP_ID/archive" -H "Authorization: Bearer $ADMIN_TOKEN" \
        && printf '%s' "$HTTP_BODY" | jq -e --arg id "$APP_ID" '.id == $id and .archived_at != null' > /dev/null 2>&1; then
        pass "archive app"
    else
        fail "archive app (HTTP $HTTP_STATUS: $(printf '%s' "$HTTP_BODY" | cut -c1-200))"
    fi

    if http_ok GET "http://localhost:$ZEROSHIP_CONTROL_PORT/internal/versions" -H "Authorization: Bearer $ZEROSHIP_CONTROL_KEY" \
        && printf '%s' "$HTTP_BODY" | jq -e ".[\"$APP_ID\"]" > /dev/null 2>&1; then
        pass "archived app retained in internal versions for database lifecycle"
    else
        fail "archived app missing from retained /internal/versions entry: $(printf '%s' "$HTTP_BODY" | cut -c1-200)"
    fi

    if http_ok DELETE "http://localhost:$ZEROSHIP_CONTROL_PORT/api/apps/$APP_ID/archive" -H "Authorization: Bearer $ADMIN_TOKEN" \
        && printf '%s' "$HTTP_BODY" | jq -e --arg id "$APP_ID" '.id == $id and .archived_at == null' > /dev/null 2>&1; then
        pass "unarchive app"
    else
        fail "unarchive app (HTTP $HTTP_STATUS: $(printf '%s' "$HTTP_BODY" | cut -c1-200))"
    fi

    if http_ok GET "http://localhost:$ZEROSHIP_CONTROL_PORT/internal/versions" -H "Authorization: Bearer $ZEROSHIP_CONTROL_KEY" \
        && printf '%s' "$HTTP_BODY" | jq -e ".[\"$APP_ID\"]" > /dev/null 2>&1; then
        pass "unarchived app remains in internal versions"
    else
        fail "unarchived app missing from /internal/versions: $(printf '%s' "$HTTP_BODY" | cut -c1-200)"
    fi
fi

# ---------------------------------------------------------------------------
# Test 3: Identity verification
# ---------------------------------------------------------------------------
echo ""
echo "=== Test 3: Identity (10 apps, each returns its own name) ==="

# The app is a raw-JS deploy under the current contract:
# `default = { fetch }` (docs/reference/zeroship-standard.md). The old
# fixture exported a bare `ping()` and was called with a JSON-RPC envelope
# at `/apps/<name>/rpc`; neither the envelope nor that path has existed
# since RPC v1 (fe571dc03).
declare -A APP_IDS

for i in $(seq 1 10); do
    name="id-$(printf '%02d' "$i")"
    id="$(create_app "$name" || true)"
    APP_IDS[$name]="$id"
    if [ -z "$id" ]; then
        fail "$name: create failed"
        continue
    fi
    deploy_js "$id" "export default { fetch() { return new Response(\"I am $name\"); } };" \
        || fail "$name: deploy failed"
done
sleep 5

ID_PASS=0
for i in $(seq 1 10); do
    name="id-$(printf '%02d' "$i")"
    [ -n "${APP_IDS[$name]:-}" ] || continue
    if http GET "http://localhost:$ZEROSHIP_GATEWAY_PORT/apps/$name/" && [ "$HTTP_STATUS" = "200" ] \
        && [ "$HTTP_BODY" = "I am $name" ]; then
        ID_PASS=$((ID_PASS + 1))
    else
        fail "$name returned HTTP $HTTP_STATUS '$(printf '%s' "$HTTP_BODY" | cut -c1-120)' (expected 200 'I am $name')"
    fi
done
[ $ID_PASS -eq 10 ] && pass "all 10 apps returned correct identity" || fail "$ID_PASS/10 correct"

# ---------------------------------------------------------------------------
# Test 4: Routing consistency (counter increments on same isolate)
# ---------------------------------------------------------------------------
echo ""
echo "=== Test 4: Routing consistency ==="

CID="$(create_app counter-app || true)"
if [ -z "$CID" ]; then
    fail "counter-app: create failed"
elif ! deploy_js "$CID" 'let counter = 0;
export default { fetch() { counter++; return new Response(JSON.stringify({ count: counter }), { headers: { "content-type": "application/json" } }); } };'; then
    fail "counter-app: deploy failed"
else
    sleep 5
    COUNTS=""
    for j in $(seq 1 10); do
        if http GET "http://localhost:$ZEROSHIP_GATEWAY_PORT/apps/counter-app/?n=$j" && [ "$HTTP_STATUS" = "200" ]; then
            COUNTS="$COUNTS $(jget "$HTTP_BODY" '.count')"
        else
            COUNTS="$COUNTS x"
            echo "    request $j: HTTP $HTTP_STATUS $(printf '%s' "$HTTP_BODY" | cut -c1-120)"
        fi
    done
    echo "  counters:$COUNTS"
    MAX_COUNT=$(printf '%s' "$COUNTS" | tr ' ' '\n' | grep -E '^[0-9]+$' | sort -rn | head -1)
    MAX_COUNT="${MAX_COUNT:-0}"
    # 3 workers x 2 threads = 6 candidate isolates. CHWBL pins the app to one
    # worker, so 10 requests land on <= 2 isolates and at least one of them
    # must be hit 3+ times. Round-robin across all six would cap at 2.
    [ "$MAX_COUNT" -ge 3 ] && pass "counter reached $MAX_COUNT (routing consistent)" \
        || fail "counter only reached $MAX_COUNT — requests are spread across isolates"
fi

# ---------------------------------------------------------------------------
# Test 5: Isolation
# ---------------------------------------------------------------------------
echo ""
echo "=== Test 5: Isolation ==="

if [ -z "${APP_IDS[id-01]:-}" ] || [ -z "${APP_IDS[id-02]:-}" ]; then
    fail "isolation: id-01/id-02 were not created"
else
    for j in $(seq 1 10); do
        http GET "http://localhost:$ZEROSHIP_GATEWAY_PORT/apps/id-01/?n=$j" >/dev/null 2>&1 || true
    done
    if http GET "http://localhost:$ZEROSHIP_GATEWAY_PORT/apps/id-02/" && [ "$HTTP_BODY" = "I am id-02" ]; then
        pass "id-02 isolated from id-01"
    else
        fail "id-02 returned HTTP $HTTP_STATUS '$(printf '%s' "$HTTP_BODY" | cut -c1-120)'"
    fi
fi

# ---------------------------------------------------------------------------
# Test 6: Auth enforcement
# ---------------------------------------------------------------------------
# WHAT THIS NO LONGER TESTS, and why. Until fe571dc03 (2026-04-30) the
# gateway called `auth::check_api_key` for `WorkerMode::Rpc` requests and
# SSR was open by design. RPC v1 replaced the key check with the compiled
# per-resource `EffectivePolicy` (`auth: anonymous|user`), and deleted the
# only call site. `crates/zeroship-gateway/src/auth.rs::check_api_key` and
# `RouteEntry.api_key_hash` outlived that call site by months, uncalled. The
# whole app-level key is now gone - the checker, the hash, the plaintext
# `zeroship.apps.api_key`, `AppRecord::api_key`, the mint, and the `X-Api-Key`
# headers this harness and its siblings used to send. Nothing reads such a
# header and nothing sends one; `db/migrations-ts/20260905000200_drop_app_api_key.ts`
# records why the platform owns no app-level key at all.
# The old assertions here "passed" regardless: they ran
# `curl -sf ... || echo rejected` and then grepped for "rejected", so any
# failure — including a gateway that was not running — satisfied them.
#
# The gate that DOES exist is asserted instead, on ONE app whose manifest
# carries two `rpc:` resources differing in exactly one field — `auth` —
# plus an anonymous URL resource. 6b alone would not prove the auth level is what
# rejects: a 401 on any `/__zeroship/v1/` path would satisfy it. 6c is the
# one-variable partner (same app, same deploy, same path shape, auth: anonymous)
# and must come back 200.
echo ""
echo "=== Test 6: Auth ==="

AUTH_ID="$(create_app authgate || true)"
if [ -z "$AUTH_ID" ]; then
    fail "authgate: create failed"
elif ! deploy_js "$AUTH_ID" 'export default { fetch() { return new Response("open"); } };' \
        '{"/[...rest]":{"auth":"anonymous","publicly_accessible":true},"rpc:secret":{"auth":"user"},"rpc:open":{"auth":"anonymous","publicly_accessible":true}}'; then
    fail "authgate: deploy failed"
else
    sleep 5
    # 6a: anonymous URL resource is reachable — proves the app is live, so a 401
    #     on 6b cannot be "the app never deployed".
    if http GET "http://localhost:$ZEROSHIP_GATEWAY_PORT/apps/authgate/" && [ "$HTTP_STATUS" = "200" ]; then
        pass "auth:anonymous resource served (HTTP 200)"
    else
        fail "auth:anonymous resource: HTTP $HTTP_STATUS $(printf '%s' "$HTTP_BODY" | cut -c1-160)"
    fi
    # 6b: rpc resource declaring auth:user, no session.
    if http POST "http://localhost:$ZEROSHIP_GATEWAY_PORT/apps/authgate/__zeroship/v1/secret" \
        -H 'Content-Type: application/json' -d '{"json":{}}' \
        && { [ "$HTTP_STATUS" = "401" ] || [ "$HTTP_STATUS" = "403" ]; }; then
        pass "rpc resource with auth:user rejected without a session (HTTP $HTTP_STATUS)"
    else
        fail "rpc auth:user returned HTTP $HTTP_STATUS (expected 401/403): $(printf '%s' "$HTTP_BODY" | cut -c1-160)"
    fi
    # 6c: the control. Identical in every respect except `auth: anonymous`.
    if http POST "http://localhost:$ZEROSHIP_GATEWAY_PORT/apps/authgate/__zeroship/v1/open" \
        -H 'Content-Type: application/json' -d '{"json":{}}' \
        && [ "$HTTP_STATUS" = "200" ]; then
        pass "rpc resource with auth:anonymous passes the same gate (HTTP 200)"
    else
        fail "rpc auth:anonymous returned HTTP $HTTP_STATUS (expected 200) — 6b's 401 may not be the auth gate: $(printf '%s' "$HTTP_BODY" | cut -c1-160)"
    fi
fi

# 6c: unknown app → 404 at the gateway.
if http GET "http://localhost:$ZEROSHIP_GATEWAY_PORT/apps/nonexistent-app/" && [ "$HTTP_STATUS" = "404" ]; then
    pass "unknown app returns 404"
else
    fail "unknown app returned HTTP $HTTP_STATUS: $(printf '%s' "$HTTP_BODY" | cut -c1-160)"
fi

# 6d/6e: the control plane's platform-token gate — the credential that
# actually guards app CRUD today.
if http POST "http://localhost:$ZEROSHIP_CONTROL_PORT/api/apps" \
    -H 'Content-Type: application/json' -d '{"name":"should-fail"}' \
    && { [ "$HTTP_STATUS" = "401" ] || [ "$HTTP_STATUS" = "403" ]; }; then
    pass "app-create without a platform token rejected (HTTP $HTTP_STATUS)"
else
    fail "app-create with no auth returned HTTP $HTTP_STATUS: $(printf '%s' "$HTTP_BODY" | cut -c1-160)"
fi

if http POST "http://localhost:$ZEROSHIP_CONTROL_PORT/api/apps" \
    -H 'Content-Type: application/json' -H 'Authorization: Bearer not-a-real-token' \
    -d '{"name":"should-fail"}' \
    && { [ "$HTTP_STATUS" = "401" ] || [ "$HTTP_STATUS" = "403" ]; }; then
    pass "app-create with a bogus platform token rejected (HTTP $HTTP_STATUS)"
else
    fail "app-create with a bogus token returned HTTP $HTTP_STATUS: $(printf '%s' "$HTTP_BODY" | cut -c1-160)"
fi

# ---------------------------------------------------------------------------
# Test 7: Cold start (on-demand loading)
# ---------------------------------------------------------------------------
echo ""
echo "=== Test 7: Cold start ==="

COLD_ID="$(create_app cold-start || true)"
if [ -z "$COLD_ID" ]; then
    fail "cold-start: create failed"
elif ! deploy_js "$COLD_ID" 'export default { fetch() { return new Response("cold-ok"); } };'; then
    fail "cold-start: deploy failed"
else
    sleep 4
    START=$(date +%s%N)
    http GET "http://localhost:$ZEROSHIP_GATEWAY_PORT/apps/cold-start/" || true
    END=$(date +%s%N)
    COLD_MS=$(( (END - START) / 1000000 ))
    if [ "$HTTP_STATUS" = "200" ] && [ "$HTTP_BODY" = "cold-ok" ]; then
        pass "cold start in ${COLD_MS}ms"
    else
        fail "cold start: HTTP $HTTP_STATUS '$(printf '%s' "$HTTP_BODY" | cut -c1-160)'"
    fi

    START=$(date +%s%N)
    http GET "http://localhost:$ZEROSHIP_GATEWAY_PORT/apps/cold-start/" || true
    END=$(date +%s%N)
    WARM_MS=$(( (END - START) / 1000000 ))
    if [ "$HTTP_STATUS" = "200" ] && [ "$HTTP_BODY" = "cold-ok" ]; then
        pass "warm request in ${WARM_MS}ms"
    else
        fail "warm request: HTTP $HTTP_STATUS '$(printf '%s' "$HTTP_BODY" | cut -c1-160)'"
    fi
fi

# ---------------------------------------------------------------------------
# Test 8: Hot deploy
# ---------------------------------------------------------------------------
echo ""
echo "=== Test 8: Hot deploy ==="

HOT_ID="$(create_app hot-deploy || true)"
if [ -z "$HOT_ID" ]; then
    fail "hot-deploy: create failed"
elif ! deploy_js "$HOT_ID" 'export default { fetch() { return new Response("v1"); } };'; then
    fail "hot-deploy: v1 deploy failed"
else
    sleep 4
    if http GET "http://localhost:$ZEROSHIP_GATEWAY_PORT/apps/hot-deploy/" && [ "$HTTP_BODY" = "v1" ]; then
        pass "v1 deployed"
    else
        fail "expected v1, got HTTP $HTTP_STATUS '$(printf '%s' "$HTTP_BODY" | cut -c1-160)'"
    fi

    if ! deploy_js "$HOT_ID" 'export default { fetch() { return new Response("v2"); } };'; then
        fail "hot-deploy: v2 deploy failed"
    else
        # Worker polls every 2s, then has to download + reload.
        v=""
        for _ in $(seq 1 10); do
            sleep 3
            http GET "http://localhost:$ZEROSHIP_GATEWAY_PORT/apps/hot-deploy/" || true
            v="$HTTP_BODY"
            [ "$v" = "v2" ] && break
        done
        [ "$v" = "v2" ] && pass "v2 hot deployed" \
            || fail "expected v2 within 30s, got HTTP $HTTP_STATUS '$(printf '%s' "$v" | cut -c1-160)'"
    fi
fi

# ---------------------------------------------------------------------------
# Test 9: Deploy edge cases (streaming early-rejection paths)
# ---------------------------------------------------------------------------
# Targets the streaming deploy handler in `crates/zeroship-control/src/api.rs`
# (`deploy()`): content-type, payload-cap and auth gates must fire
# BEFORE the body is streamed to a tmp file. Companion to Test 2's
# happy path.
echo ""
echo "=== Test 9: Deploy edge cases ==="

EDGE_APP_ID="$(create_app deploy-edge || true)"
if [ -n "$EDGE_APP_ID" ]; then pass "9.0: create edge-case app ($EDGE_APP_ID)"; else fail "9.0: create edge-case app"; fi

if [ -n "$EDGE_APP_ID" ]; then
    edge_js=$(mktemp --suffix=.js)
    edge_zship=$(mktemp --suffix=.zship)
    echo 'export default { fetch() { return new Response("edge"); } };' > "$edge_js"
    build_zship "$edge_js" "$edge_zship"
    rm -f "$edge_js"

    # --- 9.1: wrong content-type returns 415 ---
    echo "  -- 9.1: wrong content-type"
    if http POST "http://localhost:$ZEROSHIP_CONTROL_PORT/api/apps/$EDGE_APP_ID/deploy" \
        -H "Authorization: Bearer $ADMIN_TOKEN" -H 'Content-Type: application/octet-stream' \
        --data-binary "@$edge_zship" \
        && [ "$HTTP_STATUS" = "415" ] && printf '%s' "$HTTP_BODY" | grep -q "unsupported content type"; then
        pass "9.1: wrong content-type rejected with 415"
    else
        fail "9.1: expected 415 + 'unsupported content type', got $HTTP_STATUS (body: $(printf '%s' "$HTTP_BODY" | cut -c1-200))"
    fi

    # --- 9.2: body exceeding MAX_COMPRESSED_BYTES (256 MiB) returns 413 ---
    # Cap is enforced PRE-decompression, so raw bytes (no zstd needed)
    # trigger it. /dev/zero is fine — the streaming helper counts bytes.
    echo "  -- 9.2: body over 256 MiB cap"
    big_body=$(mktemp --suffix=.bin)
    dd if=/dev/zero of="$big_body" bs=1M count=257 status=none
    if http POST "http://localhost:$ZEROSHIP_CONTROL_PORT/api/apps/$EDGE_APP_ID/deploy" \
        -H "Authorization: Bearer $ADMIN_TOKEN" -H 'Content-Type: application/x-zship' \
        --data-binary "@$big_body" \
        && [ "$HTTP_STATUS" = "413" ] && printf '%s' "$HTTP_BODY" | grep -q "deploy too large"; then
        pass "9.2: oversized body rejected with 413"
    else
        fail "9.2: expected 413 + 'deploy too large', got $HTTP_STATUS (body: $(printf '%s' "$HTTP_BODY" | cut -c1-200))"
    fi
    rm -f "$big_body"

    # --- 9.3: wrong Authorization returns 401/403 ---
    echo "  -- 9.3: wrong auth on deploy"
    if http POST "http://localhost:$ZEROSHIP_CONTROL_PORT/api/apps/$EDGE_APP_ID/deploy" \
        -H "Authorization: Bearer wrong-key-12345" -H 'Content-Type: application/x-zship' \
        --data-binary "@$edge_zship" \
        && { [ "$HTTP_STATUS" = "401" ] || [ "$HTTP_STATUS" = "403" ]; }; then
        pass "9.3: wrong auth rejected with $HTTP_STATUS"
    else
        fail "9.3: expected 401/403, got $HTTP_STATUS (body: $(printf '%s' "$HTTP_BODY" | cut -c1-200))"
    fi
    rm -f "$edge_zship"

    if http_ok PUT "http://localhost:$ZEROSHIP_CONTROL_PORT/api/apps/$EDGE_APP_ID/archive" -H "Authorization: Bearer $ADMIN_TOKEN" \
        && printf '%s' "$HTTP_BODY" | jq -e --arg id "$EDGE_APP_ID" '.id == $id and .archived_at != null' >/dev/null; then
        pass "9.4: archive edge-case app"
    else
        fail "9.4: archive edge-case app (HTTP $HTTP_STATUS)"
    fi
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
REACHED_SUMMARY=1
echo ""
echo "============================================"
echo "  Results: $PASS passed, $FAIL failed"
echo "============================================"

[ $FAIL -eq 0 ] && exit 0 || exit 1
