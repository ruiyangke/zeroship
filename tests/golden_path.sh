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

# Dev-server leg ports. The zeroship dev RUNTIME binds a port of its OWN,
# separate from vite's, and `vite --port N` does not move it -- so a harness
# that only knows the vite port frees half of what it started and leaves a
# runtime holding the other half. Every port this script binds is listed here
# and freed by `cleanup`, runtime ports included.
DEV_PORT="${DEV_PORT:-3091}"          # vite, step 6
DEV_RT_PORT="${DEV_RT_PORT:-3140}"    # dev runtime, step 6 (STARTER_API_PORT)
SUP_RT="${SUP_RT:-3141}"              # dev runtime SHARED by 7a and 7b (the collision)
SUP_V1="${SUP_V1:-3142}"              # vite, 7a
SUP_V2="${SUP_V2:-3143}"              # vite, 7b
DB_V="${DB_V:-3144}"                  # vite, step 9 (db-todos data plane)
DB_RT="${DB_RT:-3145}"                # dev runtime, step 9 (DB_TODOS_API_PORT)
DEV_PORTS="$DEV_PORT $DEV_RT_PORT $SUP_RT $SUP_V1 $SUP_V2 $DB_V $DB_RT"

PASS=0; FAIL=0; PIDS=()
pass() { PASS=$((PASS+1)); echo "  ✓ $1"; }
fail() { FAIL=$((FAIL+1)); echo "  ✗ $1"; }
# `-sTCP:LISTEN` is load-bearing. Plain `lsof -ti :$PORT` matches every socket
# with that port on EITHER end, so freeing the worker's port also killed the
# GATEWAY, which merely held a client connection to it -- and the gateway dying
# turned step 8's "worker unavailable" probe into a connection failure (HTTP
# 000) that looked like a platform verdict.
free_ports() { for p in "$@"; do lsof -ti :"$p" -sTCP:LISTEN 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done; }
# Killing the recorded PIDs is not enough: `( cd x && vite )&` records the
# subshell, and the dev server in turn forks a `zeroship serve` child that is
# not in $PIDS at all. Freeing the ports by listener is what actually
# guarantees the next run of this script starts clean.
cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  sleep 1
  free_ports $DEV_PORTS
  wait 2>/dev/null || true
}
trap cleanup EXIT

# --- HTTP probes that separate "listening" from "answered 2xx" -------------
#
# `curl -sf` conflates the two: it fails on ANY non-2xx, so an app whose RPC
# returns 503 BECAUSE its runtime is down reads identically to one where vite
# never bound a socket. That points the reader at the wrong process -- and,
# with the dev-runtime supervisor added in step 7, 503 is now the EXPECTED
# answer for a dead runtime, so a `-sf` readiness loop would spin its full
# timeout and then report the one thing that is not true.
# curl PRINTS `000` and ALSO exits non-zero when it cannot connect, so the
# obvious `curl ... || echo 000` emits `000000` -- which is not equal to `000`,
# so a "did anything answer?" check reads a dead port as alive. That produced a
# false PASS ("vite IS serving HTTP (status 000000)") on the first run of this
# step. Substitute only when curl printed nothing at all.
http_status() {
  local code
  code=$(curl -s -o /dev/null -w '%{http_code}' -m 5 "$1" 2>/dev/null)
  echo "${code:-000}"
}
# 0 as soon as anything at all answers (000 == nothing listening / no response).
wait_listening() {
  local url=$1 tries=${2:-40}
  for _ in $(seq 1 "$tries"); do
    [ "$(http_status "$url")" != "000" ] && return 0
    sleep 1
  done
  return 1
}
# 0 only on a 2xx.
wait_http_ok() {
  local url=$1 tries=${2:-40} code
  for _ in $(seq 1 "$tries"); do
    code=$(http_status "$url")
    case "$code" in 2??) return 0 ;; esac
    sleep 1
  done
  return 1
}

echo "============================================"
echo "  zeroship golden path (build-local → deploy)"
echo "============================================"

# Step 1 runs `pnpm build` inside the EXAMPLE, which consumes whatever
# `sdks/vite-plugin/dist/` is already on disk. It never rebuilds the plugin. So
# an edit to the plugin's own source can be silently untested: the run is green
# about a dist that predates the change, and nothing in the output says so.
#
# Every other dev-vs-deployed harness in this directory sources
# `tests/lib/binary_freshness.sh`; this script was the only one with no
# freshness check of any kind. WARNS by default (mtime is a weak proxy for
# provenance -- a rebase touches a file without changing it); set
# ZS_FRESHNESS_STRICT=1 to refuse, which is what CI wants.
# shellcheck source=lib/binary_freshness.sh
source "$ROOT/tests/lib/binary_freshness.sh"
zs_check_artifact_freshness "$ROOT" "sdks/vite-plugin/dist/index.js" \
  "sdks/vite-plugin/src" || {
    rc=$?
    [ "$rc" = "2" ] && { fail "vite-plugin freshness check could not run"; exit 2; }
    fail "sdks/vite-plugin/dist is stale (ZS_FRESHNESS_STRICT=1)"; exit 1;
  }

# NOT covered here, and worth knowing: the RUST binaries this script starts are
# unchecked. `zs_check_binary_freshness` is the function for that and takes the
# binary directory plus names; wiring it needs this script's binary list, which
# is a separate change rather than an oversight to fix silently.

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
  --control-key "$CONTROL_KEY" --blob-store /tmp/gp-bundles --poll-interval 2 >/tmp/gp-worker.log 2>&1 & WORKER_PID=$!; PIDS+=($WORKER_PID)
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
# STARTER_API_PORT is PINNED rather than left at the 3001 default. Several
# examples share that default, so an unpinned run here inherits whatever else
# happens to be on 3001 -- and since the runtime port is not vite's, the old
# `lsof -ti :$DEV_PORT` cleanup never freed it either way.
free_ports "$DEV_PORT" "$DEV_RT_PORT"
( cd "$STARTER" && STARTER_API_PORT="$DEV_RT_PORT" ./node_modules/.bin/vite --port "$DEV_PORT" --strictPort ) >/tmp/gp-dev.log 2>&1 &
DEV_PID=$!; PIDS+=($DEV_PID)

DEV_RPC=""
DEV_RPC_URL="http://localhost:$DEV_PORT/__zeroship/v1/getMessages"
if wait_http_ok "$DEV_RPC_URL" 50; then
  DEV_RPC=$(curl -s -m 5 "$DEV_RPC_URL" 2>/dev/null || echo "")
fi

if [ -z "$DEV_RPC" ]; then
  # Say WHICH of the two failed. "never answered" was reported for both a vite
  # that never bound and a runtime that was down behind a vite serving 503s.
  if [ "$(http_status "http://localhost:$DEV_PORT/")" = "000" ]; then
    fail "vite never bound :$DEV_PORT (the dev server itself did not start)"
  else
    fail "vite is serving on :$DEV_PORT but getMessages returned $(http_status "$DEV_RPC_URL"): $(curl -s -m 5 "$DEV_RPC_URL" | head -c 200)"
  fi
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

# --- 7. A dev runtime that never starts must be LOUD, not silently looping ---
#
# THE SEAM THIS TESTS, and why it cannot live in a crate suite or a vite-plugin
# unit test. The zeroship dev runtime binds a port of its OWN (devServerPort,
# default 3001) while vite binds another. `vite --port N` moves only vite's.
# Several examples take the 3001 default, so two of them running at once is not
# an exotic setup -- it is the ordinary consequence of opening a second example.
# The second runtime then cannot bind, exits in milliseconds, and the dev server
# re-spawns it. Both halves are individually correct: the runtime is right to
# refuse a taken port, and re-spawning a crashed runtime is a real feature. The
# defect is only visible where they meet, WITH VITE STILL SERVING, which is a
# two-process, two-port condition no single-process test can construct.
#
# What made it expensive: the app presents as UP. Vite answers 200 on / the
# whole time. Measured at HEAD before the fix, with examples/error-probe holding
# the runtime port, examples/starter's own `getMessages` came back
#   404 {"message":"Method not found: getMessages","code":"NOT_FOUND"}
# -- error-probe's runtime answering about a procedure it has never had. The
# proxy connects successfully, because SOMEBODY is on that port; it is just not
# your app. Two instances of the same example are worse: HTTP 200 carrying the
# other instance's data, indistinguishable from working. So this step asserts
# the runtime's failure is REFUSED at the proxy, not merely logged.
echo "=== 7. Dev-runtime supervisor: never-starts is terminal + visible at request time ==="
# Step 6's dev server MUST be gone first, and the reason is a second collision
# that is not the one under test: two dev servers rooted in the SAME project
# directory also contend for `.zeroship/kv.redb`, and the loser dies with
# "Database already open. Cannot acquire lock." The first run of this step left
# step 6 running, so 7a -- the control -- failed on the redb lock rather than
# coming up, and every assertion after it was measuring a harness fault. The
# one variable between 7a and 7b has to be the PORT, so everything else that two
# dev servers can fight over is removed here.
kill "$DEV_PID" 2>/dev/null || true
free_ports "$DEV_PORT" "$DEV_RT_PORT" "$SUP_RT" "$SUP_V1" "$SUP_V2"
sleep 2
rm -f /tmp/gp-sup-a.log /tmp/gp-sup-b.log

# 7a. CONTROL. Identical to 7b in every respect except the one variable that
# matters: this one gets the runtime port to itself. Without it, a 7b that went
# red because the harness mis-starts dev servers would be indistinguishable from
# a 7b that went red because the supervisor is broken.
( cd "$STARTER" && STARTER_API_PORT="$SUP_RT" ./node_modules/.bin/vite --port "$SUP_V1" --strictPort ) >/tmp/gp-sup-a.log 2>&1 &
PIDS+=($!)
SUP_A_RPC="http://localhost:$SUP_V1/__zeroship/v1/getMessages"
if wait_http_ok "$SUP_A_RPC" 50; then
  pass "7a control: uncontended dev runtime on :$SUP_RT answers 2xx"
else
  fail "7a control: uncontended dev runtime never answered (status $(http_status "$SUP_A_RPC")) - 7b below cannot be trusted"
  tail -20 /tmp/gp-sup-a.log
fi

# 7b. THE COLLISION. Same example, same runtime port, different vite port.
( cd "$STARTER" && STARTER_API_PORT="$SUP_RT" ./node_modules/.bin/vite --port "$SUP_V2" --strictPort ) >/tmp/gp-sup-b.log 2>&1 &
PIDS+=($!)

# Vite must be up -- that is the premise of the whole failure ("the app presents
# as UP"), and asserting it separately is what keeps a red below from being read
# as "the second dev server never started".
if wait_listening "http://localhost:$SUP_V2/" 40; then
  pass "7b: the colliding dev server's vite IS serving HTTP (status $(http_status "http://localhost:$SUP_V2/"))"
else
  fail "7b: second dev server never bound :$SUP_V2"
  tail -20 /tmp/gp-sup-b.log
fi

# Terminal within a bounded time. Backoff is 1s+2s+4s over 4 attempts, so ~15s
# is the expected verdict; 60s is slack for a loaded machine, not a moving goal.
SUP_FATAL=0
for _ in $(seq 1 60); do
  grep -q "DEV RUNTIME FAILED TO START" /tmp/gp-sup-b.log && { SUP_FATAL=1; break; }
  sleep 1
done
if [ "$SUP_FATAL" = "1" ]; then
  pass "7b: supervisor gave up and printed the terminal banner"
else
  fail "7b: no terminal banner after 60s - the runtime is looping silently"
  echo "    restart lines so far: $(grep -c "runtime exited" /tmp/gp-sup-b.log)"
  tail -8 /tmp/gp-sup-b.log
fi

# Bounded, and STAYS bounded. Counting once proves nothing: the unbounded loop
# also has a finite count at any instant. The second reading 8s later is what
# separates "gave up" from "still going".
SUP_N1=$(grep -c "runtime exited" /tmp/gp-sup-b.log)
sleep 8
SUP_N2=$(grep -c "runtime exited" /tmp/gp-sup-b.log)
if [ "$SUP_N1" = "$SUP_N2" ] && [ "$SUP_N2" -le 4 ]; then
  pass "7b: restarts bounded at $SUP_N2 and stopped (no growth over 8s)"
else
  fail "7b: restart count $SUP_N1 -> $SUP_N2 (expected equal and <= 4) - still looping"
fi

# The half that actually reaches a creator. A log line they have scrolled past
# is not a signal; the response to the request they just made is.
SUP_CODE=$(http_status "http://localhost:$SUP_V2/__zeroship/v1/getMessages")
SUP_BODY=$(curl -s -m 5 "http://localhost:$SUP_V2/__zeroship/v1/getMessages" 2>/dev/null)
if [ "$SUP_CODE" = "503" ] && printf '%s' "$SUP_BODY" | grep -q "failed to start" \
   && printf '%s' "$SUP_BODY" | grep -q "already in use"; then
  pass "7b: server function returns 503 naming the real cause (port in use), not another app's answer"
else
  fail "7b: server function returned $SUP_CODE, body: ${SUP_BODY:0:200}"
  echo "    Expected 503 whose body names the port conflict. A 2xx here, or a 4xx"
  echo "    about a procedure this app DOES export, means the request was answered"
  echo "    by whoever else holds :$SUP_RT."
fi

# 7c. THE FEATURE THAT MUST NOT REGRESS. A runtime that came up, served, and
# then died is a DIFFERENT event from one that never started, and it must still
# be restarted. 7a's runtime has been up well past the healthy threshold; kill
# it with SIGABRT (SIGTERM/SIGKILL are the supervisor's own teardown signals and
# are ignored on purpose) and it must come back.
#
# WHAT THIS DOES NOT CATCH: a runtime killed by the OOM killer arrives as
# SIGKILL and is therefore NOT restarted. That is pre-existing behaviour, shared
# with the code before this step existed, and no assertion here would notice it.
# `-sTCP:LISTEN` again, and here it is not a tidiness point: 7a's VITE process
# holds a client connection to this port, so a bare `lsof -ti :$SUP_RT` can
# return the dev server itself and this would SIGABRT the parent rather than the
# runtime -- testing teardown while claiming to test crash recovery.
SUP_A_CHILD=$(lsof -ti :"$SUP_RT" -sTCP:LISTEN 2>/dev/null | head -1)
if [ -z "$SUP_A_CHILD" ]; then
  fail "7c: could not find the healthy runtime listening on :$SUP_RT"
else
  kill -ABRT "$SUP_A_CHILD" 2>/dev/null || true
  if wait_http_ok "$SUP_A_RPC" 45; then
    # A 2xx alone does NOT prove a restart happened. If the kill silently did
    # nothing, the ORIGINAL runtime is still serving and the very first poll
    # succeeds -- a pass that measures only that the harness failed to kill
    # anything. The pid having changed is what makes it an assertion.
    SUP_A_CHILD2=$(lsof -ti :"$SUP_RT" -sTCP:LISTEN 2>/dev/null | head -1)
    if [ -n "$SUP_A_CHILD2" ] && [ "$SUP_A_CHILD2" != "$SUP_A_CHILD" ]; then
      pass "7c: runtime respawned as a NEW process ($SUP_A_CHILD -> $SUP_A_CHILD2) and answers again"
    else
      fail "7c: :$SUP_RT still served by pid ${SUP_A_CHILD2:-none} (was $SUP_A_CHILD) - the kill did not land, so nothing was tested"
    fi
  else
    fail "7c: mid-session crash was not recovered (status $(http_status "$SUP_A_RPC")) - the legitimate restart path regressed"
    tail -15 /tmp/gp-sup-a.log
  fi
  if grep -q "DEV RUNTIME FAILED TO START" /tmp/gp-sup-a.log; then
    fail "7c: a single mid-session crash was treated as a never-started failure"
  else
    pass "7c: one mid-session crash did not count toward the give-up budget"
  fi
fi

# --- 8. The same failure on the DEPLOYED tier, measured rather than assumed ---
#
# Step 6 diffs dev vs deployed on the HAPPY path. This is the same diff on the
# failure path, and the answer is a divergence BY DESIGN rather than a defect:
# there is no supervisor on the deployed side at all, so "the app's runtime is
# unavailable" is answered by the gateway, not by a dev-server middleware.
# Recording what it actually says is the point -- the alternative is assuming
# it is fine, which is what left the dev side opaque for as long as it was.
#
# Runs LAST because it stops the worker.
echo "=== 8. Deployed tier: the same RPC when the app's runtime is unavailable ==="
DEP_URL="http://localhost:$GATE_PORT/apps/$APP_NAME/__zeroship/v1/getMessages"
DEP_OK_CODE=$(curl -s -o /dev/null -w '%{http_code}' -m 5 "$DEP_URL" -H "X-Api-Key: $API_KEY")
# Kill the worker BY PID. Freeing the port by listener is the safer idiom for
# dev ports, but here the gateway is a CLIENT of this port and an over-broad
# match takes it down too -- which replaces the platform's answer with a
# connection failure and makes this step measure nothing.
kill -9 "$WORKER_PID" 2>/dev/null || true
sleep 3
DEP_DOWN_CODE=$(curl -s -o /dev/null -w '%{http_code}' -m 10 "$DEP_URL" -H "X-Api-Key: $API_KEY")
DEP_DOWN_BODY=$(curl -s -m 10 "$DEP_URL" -H "X-Api-Key: $API_KEY" 2>/dev/null)
echo "    deployed, worker up   : HTTP $DEP_OK_CODE"
echo "    deployed, worker down : HTTP $DEP_DOWN_CODE  body: ${DEP_DOWN_BODY:0:160}"
echo "    dev, runtime down     : HTTP $SUP_CODE  body: ${SUP_BODY:0:160}"
case "$DEP_DOWN_CODE" in
  5??) pass "deployed tier fails CLOSED (HTTP $DEP_DOWN_CODE) when the runtime is unavailable" ;;
  2??) fail "deployed tier answered 2xx (HTTP $DEP_DOWN_CODE) with no worker - it served something stale" ;;
  *)   fail "deployed tier returned HTTP $DEP_DOWN_CODE with no worker; expected a 5xx" ;;
esac
# The two tiers need not use the same status text -- one is a gateway, the other
# a dev proxy -- but they must AGREE that the request did not succeed. A tier
# that returns 2xx while its backend is gone is the failure this whole ticket is
# about, just moved one layer out.
if [ "${SUP_CODE:0:1}" = "5" ] && [ "${DEP_DOWN_CODE:0:1}" = "5" ]; then
  pass "dev and deployed AGREE: runtime unavailable is a 5xx on both tiers"
else
  fail "dev($SUP_CODE) and deployed($DEP_DOWN_CODE) DIVERGE on runtime-unavailable"
fi

# --- 9. The data plane: a migration's own field names must reach the DB ---
#
# THE SEAM THIS TESTS. Everything above drives RPC and assets; no step touches
# `env.db`, because examples/starter declares no migrations at all. That left
# the longest chain in the platform uncovered end to end:
#
#   committed migration  ->  engine DDL      (the column, spelled verbatim)
#                        ->  descriptor      (schema.runtime.json)
#                        ->  generated env.db.ts
#                        ->  installSchema   (JS field -> wire column)
#                        ->  the actual SQL
#
# Each link reads correctly alone. The defect was in the JOIN: installSchema
# snake_cased the field on the way out while every other link passed the
# authored name through, so an app whose migration declared `userId` got a
# table with a `userId` column and a data plane asking for `user_id`:
#
#     db: table default.todos has no column named user_id
#
# No crate suite can see that -- the two halves live in a Rust plugin and a JS
# SDK, and each is self-consistent. It stayed invisible for a second reason
# worth recording: all 40+ platform migrations author snake_case, on which the
# mapping is the identity, so the only fixture that could ever expose it is one
# with a case boundary in a field name. db-hitcounter's single column is
# `path`. examples/db-todos is the first.
#
# The second assertion covers the same seam for relation metadata: a migration
# declares its FK as `t.text().references("users","id")` (the vendored engine
# DSL has no t.ref()), and `with:` used to reject exactly that shape.
#
# This drives the DEV tier only, deliberately. Step 3 above deploys a .zship but
# never runs `migrated`, so the deployed stack in this script has no creator
# schema to query -- asserting against it would test the absence of a migration,
# not the presence of a column. Extending step 3 to apply creator migrations is
# what would let this become a dev-vs-deployed comparison.
echo "=== 9. Data plane: migration field names survive to the database ==="
TODOS="$ROOT/examples/db-todos"
free_ports "$DB_V" "$DB_RT"
( cd "$TODOS" && DB_TODOS_API_PORT="$DB_RT" ./node_modules/.bin/vite --port "$DB_V" --strictPort ) \
  >/tmp/gp-dbtodos.log 2>&1 &
PIDS+=($!)

DB_RPC="http://localhost:$DB_RT/__zeroship/v1"
db_call() { curl -sS -m 10 -X POST -H 'content-type: application/json' "$DB_RPC/$1" -d "{\"json\":$2}"; }

# The wire ids used here are the PUBLISHED ids (`users.seed`, `todos.create`),
# which are NOT the export names (`seedUser`, `createTodo`) -- see the note at
# step 1b. A call against an export name answers `Method not found` forever.
#
# Readiness and the seed are SEPARATE steps on purpose. Folding them together
# (retrying `users.seed` until it returns an id) looks tidier and is wrong: it
# retries through real errors as though they were "not up yet", so a genuine
# defect is reported as "the runtime never came up" with the actual cause
# buried. Observed while building this: the loop reported a startup timeout
# when what had happened was `UNIQUE constraint failed: users.handle`.
#
# `users.handle` and `users.email` are UNIQUE, so a fixed literal makes this
# leg pass exactly once per database and fail on every re-run. The identity is
# per-run.
GP_TAG="gp-$$-${RANDOM}"
DB_UP=0
for _ in $(seq 1 60); do
  if [ "$(http_status "http://localhost:$DB_RT/__zeroship/v1/todos.list")" != "000" ]; then
    DB_UP=1; break
  fi
  sleep 1
done

if [ "$DB_UP" -ne 1 ]; then
  fail "db-todos dev runtime never bound :$DB_RT (data-plane leg could not run)"
  tail -20 /tmp/gp-dbtodos.log
else
  SEED=$(db_call users.seed "{\"email\":\"$GP_TAG@example.com\",\"name\":\"GP\",\"handle\":\"$GP_TAG\"}")
  GP_ID=$(printf '%s' "$SEED" | sed -nE 's/.*"id":"([^"]+)".*/\1/p')
  if [ -z "$GP_ID" ]; then
    fail "users.seed did not return an id: ${SEED:0:200}"
  else
    pass "seeded a user through env.db"

    # The camelCase field is the whole point: `userId` must arrive as the
    # column the migration created, not a snake_cased rewrite of it.
    MADE=$(db_call todos.create "{\"userId\":\"$GP_ID\",\"title\":\"golden path\",\"priority\":\"low\"}")
    case "$MADE" in
      *'"id":'*) pass "insert with a camelCase field (userId) reached the migration's column" ;;
      *) fail "insert with a camelCase field FAILED: ${MADE:0:220}
    This is the descriptor-name-vs-column seam. A body naming a column that
    'has no column named ...' means a link in the chain renamed the field." ;;
    esac

    # A foreign key DECLARED IN A MIGRATION must be usable as a relation.
    JOINED=$(db_call todos.listWithUser "{\"userId\":\"$GP_ID\"}")
    # Asserting on THIS run's email, not a literal: the joined row must be the
    # user this run seeded. Matching a fixed address would pass on a row some
    # earlier run left behind.
    case "$JOINED" in
      *"$GP_TAG@example.com"*) pass "with: eager-loaded across a migration-declared FK" ;;
      *) fail "with: did not join across the migration's FK: ${JOINED:0:220}
    'is not a t.ref field' here means relation loading is gated on a type
    token the migration-first pipeline cannot produce." ;;
    esac
  fi
fi

echo ""
echo "============================================"
echo "  golden path: $PASS passed, $FAIL failed"
echo "  MUTATION: MUTATE_DEV_DIVERGE=1 must turn step 6 RED"
echo "============================================"
[ "$FAIL" -eq 0 ]
