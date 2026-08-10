#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Golden path — the "build locally → deploy → run" chain, end-to-end, with a
# REAL vite-built app (examples/starter), not a hand-packed fixture.
#
# This is the canonical creator flow under the build-local strategy: an AI
# coding agent (Claude Code / Codex) builds a zeroship app on the creator's
# machine, `pnpm build` produces dist/app.zship, and `zeroship deploy` ships
# it to the platform — then the gateway serves it.
#
# Proves: vite-plugin build → .zship → control-plane deploy → worker load →
# gateway serve (static assets) + (best-effort) an RPC round-trip.
#
# Prereqs (see docs/runbooks/local-dev.md):
#   - service/CLI binaries: cargo build --release -p zeroship-control -p zeroship-worker -p zeroship-gateway -p zeroship --bins
#   - migration binary: cargo build --release -p zeroship-migrate-adapter --features platform-cli --bin zeroship-platform-migrate
#   - a Postgres reachable at $DATABASE_URL (default: the compose instance on :5440)
#   - examples/starter deps installed (pnpm install) so `pnpm build` works
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release"
# Dedicated, freshly-migrated DB per run (isolated from the shared `zeroship`
# db) so the run is self-contained + reproducible and never re-provisions a
# stale app.
# The container the dev compose stack creates for the Postgres on :5440. Override
# PG_CONTAINER when running against a differently-named container.
PG_CONTAINER="${PG_CONTAINER:-compose-postgres-1}"
PG_USER="${PG_USER:-postgres}"
PG_DB="${PG_DB:-zeroship_golden}"
DB_URL="${DATABASE_URL:-postgres://postgres:zeroship@localhost:5440/$PG_DB}"
# Distinct ports so this never clashes with a running dev stack.
CONTROL_PORT="${CONTROL_PORT:-9390}"
WORKER_PORT="${WORKER_PORT:-8390}"
GATE_PORT="${GATE_PORT:-8300}"
CONTROL_KEY="gp-ck"
MASTER_KEY="gp-mk"
# Local dev: run the platform without the production secret set (signing keys,
# worker key, …). NEVER use this outside local dev. Mirrors tests/m0_gate.sh.
export ZEROSHIP_DEV_INSECURE=1
export WORKER_KEY="${WORKER_KEY:-golden-path-worker-key-0123456789abcdef}"
APP_NAME="starter"
STARTER="$ROOT/examples/starter"
ZSHIP="$STARTER/dist/app.zship"

PASS=0; FAIL=0; PIDS=()
pass() { PASS=$((PASS+1)); echo "  ✓ $1"; }
fail() { FAIL=$((FAIL+1)); echo "  ✗ $1"; }
cleanup() { for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done; wait 2>/dev/null || true; }
trap cleanup EXIT

echo "============================================"
echo "  zeroship golden path (build-local → deploy)"
echo "============================================"

# --- 1. Build the starter (real vite-plugin → .zship) ---
echo "=== 1. Build examples/starter (pnpm build → dist/app.zship) ==="
( cd "$STARTER" && pnpm build ) >/tmp/gp-build.log 2>&1
[ -f "$ZSHIP" ] && pass "built $(basename "$ZSHIP") ($(du -k "$ZSHIP" | cut -f1)KB)" || { fail "build produced no app.zship"; tail -20 /tmp/gp-build.log; exit 1; }

# --- 1b. The artifact is INSPECTABLE, and declares the procedures we shipped ---
#
# There is no `zeroship inspect` command (removed in the artifact-layout
# redesign), so the only way a creator sees inside their own build is plain
# `tar`. That makes the archive layout a creator-facing contract even though no
# first-party command depends on it: change it, and the only available way to
# answer "what are my actual wire ids" stops working.
#
# That question is not hypothetical. Wire ids are configurable and need not
# match export names - `examples/db-todos` exports `seedUser` but publishes
# `users.seed`, and calling the export name returns a bare
# `Method not found: seedUser` with no hint that an id mapping exists. The
# manifest is where the real answer lives.
#
# WHAT THIS DOES NOT CATCH, and it is the failure that has actually bitten:
# a module missing its `"use server"` directive builds clean, reports 0 server
# functions, and STILL emits a manifest declaring every RPC - so a manifest
# listing `getMessages` is NOT evidence that `getMessages` is callable. This
# asserts the artifact is readable and says what we expect; step 5 is what
# proves a procedure actually answers.
zship_rpc_ids() {
  local mf
  mf="$(tar --zstd -xOf "$1" manifest.json 2>/dev/null)" || return 3
  [ -n "$mf" ] || return 3
  printf '%s' "$mf" | node -e '
    let s=""; process.stdin.on("data",d=>s+=d).on("end",()=>{
      let m; try { m = JSON.parse(s); } catch { process.exit(4); }
      const ids = Object.keys(m.resources||{}).filter(k=>k.startsWith("rpc:")).sort();
      if (!ids.length) process.exit(5);
      process.stdout.write(ids.join(","));
    });'
}
GP_IDS="$(zship_rpc_ids "$ZSHIP")"; GP_IDS_RC=$?
if [ "$GP_IDS_RC" -ne 0 ]; then
  fail "manifest not readable from the .zship (rc=$GP_IDS_RC: 3=no manifest.json, 4=unparseable, 5=no rpc resources)"
elif [ "$GP_IDS" = "rpc:addMessage,rpc:getMessages" ]; then
  pass "artifact inspectable via tar; manifest declares $GP_IDS"
else
  fail "manifest rpc ids changed: expected rpc:addMessage,rpc:getMessages got '$GP_IDS'"
fi

# --- 2. Bring up the stack (control + worker + gateway) ---
echo "=== 2. Bring up the stack ==="
for p in $CONTROL_PORT $WORKER_PORT $GATE_PORT; do lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done
rm -rf /tmp/gp-bundles

# Fresh dedicated DB + the full platform schema (db/migrations-ts JS DSL,
# recorded to transient IR by zeroship-platform-migrate).
echo "  migrating a fresh $PG_DB ..."
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "DROP DATABASE IF EXISTS $PG_DB WITH (FORCE)" >/dev/null 2>&1 || \
  docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "DROP DATABASE IF EXISTS $PG_DB" >/dev/null 2>&1 || true
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "CREATE DATABASE $PG_DB" >/dev/null 2>&1 || true
"$BIN/zeroship-platform-migrate" \
    --database-url "$DB_URL" \
    --migrations-dir "$ROOT/db/migrations-ts" \
    --project-schema zeroship \
    --project-id zeroship >/tmp/gp-migrate.log 2>&1 \
    || { fail "platform migrations failed"; tail -20 /tmp/gp-migrate.log; exit 1; }
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -tAc "select to_regclass('zeroship.apps')" 2>/dev/null | grep -q apps \
  && pass "schema migrated (fresh $PG_DB)" || { fail "schema missing after migrate"; exit 1; }

"$BIN/zeroship-control" --port "$CONTROL_PORT" --db "$DB_URL" --blob-store /tmp/gp-bundles \
  --control-key "$CONTROL_KEY" --master-key "$MASTER_KEY" >/tmp/gp-control.log 2>&1 & PIDS+=($!)
sleep 3
"$BIN/zeroship-worker" --port "$WORKER_PORT" --worker-threads 2 --control "http://localhost:$CONTROL_PORT" \
  --control-key "$CONTROL_KEY" --blob-store /tmp/gp-bundles --poll-interval 2 >/tmp/gp-worker.log 2>&1 & PIDS+=($!)
sleep 2
# The gateway refuses to boot without a broker secret; it signs the RP-initiated
# login handshake, so there is no safe default and no dev fallback.
GATE_BROKER_SECRET=/tmp/gp-gate-broker-secret
openssl rand -base64 48 > "$GATE_BROKER_SECRET"
chmod 600 "$GATE_BROKER_SECRET"
"$BIN/zeroship-gate" --port "$GATE_PORT" --control "http://localhost:$CONTROL_PORT" \
  --control-key "$CONTROL_KEY" --workers "http://localhost:$WORKER_PORT" --blob-store /tmp/gp-bundles \
  --gateway-broker-secret-file "$GATE_BROKER_SECRET" --poll-interval 2 >/tmp/gp-gate.log 2>&1 & PIDS+=($!)
sleep 3

curl -sf "http://localhost:$CONTROL_PORT/health" >/dev/null && pass "control healthy" || { fail "control down"; tail -20 /tmp/gp-control.log; exit 1; }
curl -sf "http://localhost:$WORKER_PORT/health"  >/dev/null && pass "worker healthy"  || { fail "worker down";  tail -20 /tmp/gp-worker.log; exit 1; }
curl -sf "http://localhost:$GATE_PORT/health"    >/dev/null && pass "gateway healthy" || { fail "gateway down"; tail -20 /tmp/gp-gate.log; exit 1; }

# --- 3. Create app + deploy the real .zship ---
echo "=== 3. Create app + deploy ==="
TOKEN="${ZEROSHIP_TOKEN:-}"
if [ -n "$TOKEN" ]; then
  APP=$(curl -sf -X POST "http://localhost:$CONTROL_PORT/api/apps" -H 'Content-Type: application/json' \
    -H "Authorization: Bearer $TOKEN" -d "{\"name\":\"$APP_NAME\"}")
  APP_ID=$(echo "$APP" | jq -r '.id'); API_KEY=$(echo "$APP" | jq -r '.api_key')
  [ -n "$APP_ID" ] && [ "$APP_ID" != "null" ] && pass "created app ($APP_ID)" || { fail "create app: $APP"; exit 1; }

  DEPLOY=$("$BIN/zeroship" deploy "$ZSHIP" --app="$APP_ID" --control="http://localhost:$CONTROL_PORT" --token="$TOKEN" 2>&1)
  echo "$DEPLOY" | grep -q "deploy_hash" && pass "deployed real vite .zship" || { fail "deploy: $DEPLOY"; exit 1; }
else
  OUT=$("$BIN/dev-provision" --db "$DB_URL" --blob-store /tmp/gp-bundles --name "$APP_NAME" --zship "$ZSHIP")
  APP_ID=$(echo "$OUT" | awk -F= '$1 == "app_id" { print $2 }')
  API_KEY=$(echo "$OUT" | awk -F= '$1 == "api_key" { print $2 }')
  [ -n "$APP_ID" ] && [ -n "$API_KEY" ] && pass "dev-provisioned app ($APP_ID)" || { fail "dev-provision: $OUT"; exit 1; }
fi
sleep 4  # gateway route-sync poll

# --- 4. The chain works: gateway serves the deployed app ---
echo "=== 4. Live: gateway serves the deployed app ==="
INDEX=$(curl -sf "http://localhost:$GATE_PORT/apps/$APP_NAME/" -H "X-Api-Key: $API_KEY" 2>/dev/null || echo "")
echo "$INDEX" | grep -qi "<!doctype html" && pass "GET / serves the app index.html" || fail "index.html not served (got: ${INDEX:0:80})"

# the hashed JS asset referenced by index.html
ASSET=$(echo "$INDEX" | grep -oE '/assets/[A-Za-z0-9._-]+\.js' | head -1)
if [ -n "$ASSET" ]; then
  code=$(curl -s -o /dev/null -w '%{http_code}' "http://localhost:$GATE_PORT/apps/$APP_NAME$ASSET" -H "X-Api-Key: $API_KEY")
  [ "$code" = "200" ] && pass "client JS asset served ($ASSET → 200)" || fail "asset $ASSET → $code"
fi

# --- 5. RPC round-trip: the deployed app's SERVER FUNCTION actually executes ---
# vite-app RPCs are at /__zeroship/v1/<wireId> (GET ?input= for queries), the
# same path the browser client uses; through the path-routed gateway that's
# /apps/<name>/__zeroship/v1/<wireId>. getMessages takes no input.
echo "=== 5. RPC round-trip (server function executes) ==="
RPC=$(curl -s "http://localhost:$GATE_PORT/apps/$APP_NAME/__zeroship/v1/getMessages" \
  -H "X-Api-Key: $API_KEY" 2>/dev/null || echo "")
# Assert the SHAPE, not one substring. `grep -q "Build locally"` passed on any
# response that happened to contain that text -- an error envelope quoting the
# seed data, a truncated array, a single message, an object instead of a list.
# The dev-vs-deployed comparison below is structural, but it is RELATIVE: it
# cannot see a defect both tiers share. This is the absolute half, and it is the
# only assertion here that pins what the deployed worker actually returned.
#
# examples/starter/src/server.ts seeds exactly two messages, ids 1 and 2, each
# with a numeric createdAt. Asserting the count is what catches a partial
# result; asserting createdAt is a number is what catches the field arriving as
# a string through a serialiser change.
rpc_shape() {
  node -e '
const raw = process.argv[1];
let v;
try { v = JSON.parse(raw); } catch { console.log("not JSON"); process.exit(0); }
if (v && !Array.isArray(v) && v.json !== undefined) v = v.json;
if (!Array.isArray(v)) { console.log("not an array"); process.exit(0); }
if (v.length !== 2) { console.log(`expected 2 messages, got ${v.length}`); process.exit(0); }
const ids = v.map((m) => m && m.id).join(",");
if (ids !== "1,2") { console.log(`expected ids 1,2 got ${ids}`); process.exit(0); }
if (!v.every((m) => typeof m.text === "string" && m.text.length > 0)) {
  console.log("a message has no text"); process.exit(0);
}
if (!v.every((m) => typeof m.createdAt === "number")) {
  console.log("createdAt is not a number on every message"); process.exit(0);
}
if (!v[0].text.includes("Build locally")) { console.log("seed text missing"); process.exit(0); }
console.log("ok");
' "$1"
}
shape=$(rpc_shape "$RPC")
if [ "$shape" = "ok" ]; then
  pass "getMessages returned the 2 seeded messages, ids 1,2, with numeric createdAt"
else
  echo "  RPC response: ${RPC:0:200}"
  fail "RPC round-trip: $shape"
fi

# --- 6. The DEV half of the golden path, and it must agree with deployed ---
#
# Everything above proves the deployed side. The golden path a creator actually
# follows starts one step earlier -- `pnpm dev`, edit, refresh -- and this
# script never ran it, so scenario 1 in docs/pilot/e2e-scenarios.md read "dev
# server run repeatedly, not recorded as a scenario". Running it is half the
# point; the other half is that it must answer the SAME as the deployed app,
# because dev and deployed are different backends behind one contract and a
# divergence between them is invisible to any test that only drives one.
#
# `vite` is spawned directly rather than via `pnpm dev` so the PID is the dev
# server itself and cleanup cannot leave an orphan behind a package-manager
# wrapper.
echo "=== 6. Dev server: pnpm dev serves the same app, and agrees with deployed ==="
DEV_PORT="${DEV_PORT:-3091}"
lsof -ti :"$DEV_PORT" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
( cd "$STARTER" && ./node_modules/.bin/vite --port "$DEV_PORT" --strictPort ) >/tmp/gp-dev.log 2>&1 &
PIDS+=($!)

DEV_RPC=""
for _ in $(seq 1 25); do
  DEV_RPC=$(curl -sf -m 3 "http://localhost:$DEV_PORT/__zeroship/v1/getMessages" 2>/dev/null || echo "")
  [ -n "$DEV_RPC" ] && break
  sleep 2
done

if [ -z "$DEV_RPC" ]; then
  fail "dev server never answered getMessages on :$DEV_PORT"
  tail -20 /tmp/gp-dev.log
else
  pass "dev server executed the same server function"

  # `createdAt` is dropped from BOTH sides before comparing, and nothing else is.
  #
  # examples/starter/src/server.ts:28 seeds its messages with
  # `createdAt: Date.now() - 60_000`, evaluated when the module is first
  # imported. The dev server and the worker are separate processes that import
  # it at different moments, so the two payloads can never be byte-identical no
  # matter how correct the platform is. The first version of this check compared
  # the raw bodies and reported a divergence of 1786274106211 vs 1786274103764 --
  # a 2.4-second gap, which is process start time, not a platform defect.
  #
  # So this is normalisation of a field that is volatile BY CONSTRUCTION, not a
  # weakened assertion. What IS still compared: the envelope shape, every id and
  # text, their order, and the array length. What is NOT compared, and would be
  # missed: any divergence confined to createdAt itself.
  strip_volatile() { printf '%s' "$1" | sed -E 's/"createdAt":[0-9]+/"createdAt":<t>/g'; }
  DEV_CMP=$(strip_volatile "$DEV_RPC")
  DEP_CMP=$(strip_volatile "$RPC")

  # Set to 1 to corrupt the dev body before comparing. The comparison below is
  # the only assertion in this script that can catch a dev-vs-deployed
  # divergence, so a version of it that never fires would be worse than absent.
  [ "${MUTATE_DEV_DIVERGE:-0}" = "1" ] && DEV_CMP="${DEV_CMP}__mutated__"

  if [ "$DEV_CMP" = "$DEP_CMP" ]; then
    pass "dev and deployed agree on getMessages (identical apart from createdAt)"
  else
    fail "dev and deployed DIVERGE on the same RPC"
    echo "    dev      : ${DEV_CMP:0:160}"
    echo "    deployed : ${DEP_CMP:0:160}"
    echo "    A divergence here is the finding, not a flaky test: both sides ran the"
    echo "    same source through different backends and disagreed. createdAt is"
    echo "    already normalised out, so this is not process-start skew."
  fi
fi

echo ""
echo "============================================"
echo "  golden path: $PASS passed, $FAIL failed"
echo "  MUTATION: MUTATE_DEV_DIVERGE=1 must turn step 6 RED"
echo "============================================"
[ "$FAIL" -eq 0 ]
