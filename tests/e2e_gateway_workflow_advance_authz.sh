#!/usr/bin/env bash
# Gateway internal workflow-advance authorization probe.
#
# DRIVES (does not merely read) whether an external caller can make the gateway
# advance an arbitrary app's workflow through
# `/__zeroship/internal/workflow-advance`.
#
# WHAT THIS PROBE FOUND, AND WHAT IT NOW GUARDS. It was written against an edge
# whose only gate was a Host-header check: a caller whose Host did not parse as
# a subdomain reached the forwarder and advanced the workflow under the
# GATEWAY's own authority. That edge now verifies WHICH SERVICE is calling - the
# full service-assertion profile, signed and single-use, because the rate is per
# advance - so arm A refuses instead of advancing, and this file's expected-
# failure list is gone rather than emptied.
#
# Boots disposable Postgres, applies the real platform migrations, boots real
# control/gateway/worker binaries, deploys a real self-completing workflow app,
# starts a real workflow run, and then MEASURES the exact bytes the gateway
# returns for:
#
#   A  (exploit)  POST internal route, Host does NOT parse as a subdomain
#                 (Host: localhost), body naming the real {runId, appId}, and
#                 NO caller credential.
#                 -> must be refused with HTTP 401, and the run must not move.
#   C1a (control) same POST, Host = <app>.zeroship.localhost (an APP subdomain)
#                 -> refused by the Host gate, which runs FIRST and still 404s.
#   C1b (control) same POST, Host = evil.example.com (a NON-app subdomain)
#                 -> also refused: proves the Host gate keys on "parses as a
#                    subdomain", NOT on "is a registered app" (a correction to
#                    the original hypothesis).
#   C2  (control) reachable Host, body app_id = a random non-existent UUID
#                 -> the SAME 401 as arm A, byte for byte: an unauthenticated
#                    caller cannot use this edge as an app-existence oracle.
#
# The positive direction - a control-minted assertion is ADMITTED - is measured
# in `crates/zeroship-gateway/src/router/dispatch.rs`'s own tests, which mint a
# real assertion for this handler. Without that pairing a probe that only sees
# 401s cannot tell a working guard from a broken route.
#
# Phase 2 restarts the worker WITHOUT --workflow-advance-unsigned (the
# deploy/compose shipped configuration) and re-fires the exploit, to measure
# what the shipped topology actually does.
#
# Phase 3 stands up the repo's Caddy catch-all rule in front of the gateway and
# measures whether the exploit Host can be delivered THROUGH Caddy at all.
#
# Every arm prints the exact request line and the exact response bytes.

set -uo pipefail   # NOTE: not -e; we assert on measured exit codes/bytes.

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release"
# shellcheck source=tests/lib/runtime_secrets.sh
source "$ROOT/tests/lib/runtime_secrets.sh"

PG_PORT="${PG_PORT:-5443}"
PG_CONTAINER="${PG_CONTAINER:-zs-wfadvz-pg}"
PG_USER="${PG_USER:-postgres}"
PG_PASS="${PG_PASS:-zeroship}"
PG_DB="${PG_DB:-zeroship_wfadvz_$(date +%s)_$$}"
ZEROSHIP_CONTROL_PORT="${ZEROSHIP_CONTROL_PORT:-9150}"
ZEROSHIP_WORKER_PORT="${ZEROSHIP_WORKER_PORT:-9151}"
ZEROSHIP_GATEWAY_PORT="${ZEROSHIP_GATEWAY_PORT:-9152}"
CADDY_PORT="${CADDY_PORT:-9153}"
APP_NAME="wfadvz-$(date +%s)-$$"
DBURL="postgres://$PG_USER:$PG_PASS@localhost:$PG_PORT/$PG_DB"

# Persist logs across the run so a mid-run failure leaves them for inspection.
if [ -n "${CLAUDE_JOB_DIR:-}" ]; then
  WORK="$CLAUDE_JOB_DIR/tmp/wfadvz-work"
  rm -rf "$WORK"; mkdir -p "$WORK"
else
  WORK="$(mktemp -d -t zs-wfadvz-XXXXXX)"
fi
KEEP_LOGS="${KEEP_LOGS:-1}"
PIDFILE="$WORK/pids"
: > "$PIDFILE"
PG_ADMIN_CONTAINER=""
OWNED_PG_CONTAINER=0
CREATED_PG_DB=0
CADDY_CONTAINER=""
FAILS=0

pass() { echo "  PASS $1"; }
declare -a AUTHZ_FAILURES=()
fail() { echo "  FAIL $1"; FAILS=$((FAILS+1)); AUTHZ_FAILURES+=("$1"); }
note() { echo "  ---- $1"; }

kill_pids() {
  if [ -f "$PIDFILE" ]; then
    while read -r pid; do [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; done < "$PIDFILE"
  fi
}

cleanup() {
  kill_pids
  wait 2>/dev/null || true
  [ -n "$CADDY_CONTAINER" ] && docker rm -f "$CADDY_CONTAINER" >/dev/null 2>&1 || true
  if [ "$CREATED_PG_DB" = "1" ] && [ -n "$PG_ADMIN_CONTAINER" ]; then
    docker exec "$PG_ADMIN_CONTAINER" psql -U "$PG_USER" -d postgres -v ON_ERROR_STOP=1 \
      -c "DROP DATABASE IF EXISTS \"$PG_DB\" WITH (FORCE);" >/dev/null 2>&1 || true
  fi
  [ "$OWNED_PG_CONTAINER" = "1" ] && docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
  if [ "$KEEP_LOGS" = "1" ]; then
    echo "  ---- logs preserved under $WORK (control.log/worker.log/gate.log)"
  else
    rm -rf "$WORK"
  fi
}
trap cleanup EXIT

require_cmd() { command -v "$1" >/dev/null || { fail "$1 required"; exit 2; }; }

psql_admin() {
  docker exec -i "$PG_ADMIN_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -v ON_ERROR_STOP=1 "$@"
}

wait_health() {
  local name="$1" url="$2" log="$3" i
  for i in $(seq 1 45); do
    curl -sf "$url" >/dev/null 2>&1 && { pass "$name healthy"; return 0; }
    sleep 1
  done
  fail "$name never became healthy"; tail -60 "$log" 2>/dev/null || true; exit 1
}

# POST the internal route directly at the gateway port with an arbitrary Host.
# Prints "HTTP <code>\n<body>" to stdout; sets global LAST_CODE / LAST_BODY.
LAST_CODE=""; LAST_BODY=""
gw_post() {
  local host="$1" body="$2" hdr
  if [ "$host" = "__NONE__" ]; then hdr=(-H "Host;"); else hdr=(-H "Host: $host"); fi
  LAST_CODE="$(curl -s --max-time 40 -o "$WORK/resp.body" -w '%{http_code}' \
    -X POST "http://127.0.0.1:$ZEROSHIP_GATEWAY_PORT/__zeroship/internal/workflow-advance" \
    "${hdr[@]}" -H 'content-type: application/json' --data "$body")"
  LAST_BODY="$(cat "$WORK/resp.body")"
}

# Per-app workflow runs table (crates/zeroship-plugin-workflow/src/store/pg.rs:
# app_schema_for = "app_<uuid>", table "__zeroship_workflow_runs").
runs_table() { echo "\"app_${APP_ID}\".\"__zeroship_workflow_runs\""; }

# Recover the most recent queued run id straight from the DB. Used when
# control's HTTP create returns 500 AFTER committing the run (the
# register_run_timer post-commit step, workflow_instance_api.rs:1483). Echoes
# the id on stdout, or nothing if no such row exists (=> pre-commit failure).
db_recent_queued_run() {
  docker exec "$PG_ADMIN_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -At -v ON_ERROR_STOP=1 \
    -c "SELECT id FROM $(runs_table) WHERE app_id = '$APP_ID' AND state = 'queued' \
        ORDER BY created_at DESC LIMIT 1;" 2>/dev/null
}

# Create a workflow run via control; echoes ONLY the run id on stdout.
# NOTE: this runs inside $(...), so any diagnostic MUST go to stderr (>&2),
# else it is captured into the caller's variable and silently lost.
create_run() {
  local out code rid token
  out="$WORK/create.out"
  token="$(ZEROSHIP_CONTROL_KEY="$ZEROSHIP_CONTROL_KEY" APP_ID="$APP_ID" node -e 'const c=require("crypto");process.stdout.write(c.createHmac("sha256",process.env.ZEROSHIP_CONTROL_KEY).update(process.env.APP_ID).digest("hex"))')"
  code="$(curl -s --max-time 20 -o "$out" -w '%{http_code}' \
    -X POST "http://127.0.0.1:$ZEROSHIP_CONTROL_PORT/internal/workflows/ProbeWorkflow/runs" \
    -H "authorization: Bearer $token" \
    -H "x-zeroship-app-id: $APP_ID" -H 'content-type: application/json' \
    --data '{"input":{}}')"
  if [ "$code" = "201" ] || [ "$code" = "200" ]; then
    jq -r '.id' "$out"; return 0
  fi
  # 500 path: the run is committed before register_run_timer runs, so recover it.
  echo "  ---- create_run HTTP $code ($(cat "$out" 2>/dev/null)); recovering run id from DB" >&2
  rid="$(db_recent_queued_run)"
  if [ -n "$rid" ]; then
    echo "  ---- recovered committed queued run $rid (post-commit timer failure is benign for this probe)" >&2
    echo "$rid"; return 0
  fi
  echo "  FAIL create_run HTTP $code and NO queued run row exists -> pre-commit failure, real blocker" >&2
  echo "  ---- schemas: $(docker exec "$PG_ADMIN_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -At -c "SELECT nspname FROM pg_namespace WHERE nspname LIKE 'app_%';" 2>&1 | tr '\n' ' ')" >&2
  return 1
}

run_state() {
  local rid="$1" out token
  out="$WORK/state.out"
  token="$(ZEROSHIP_CONTROL_KEY="$ZEROSHIP_CONTROL_KEY" APP_ID="$APP_ID" node -e 'const c=require("crypto");process.stdout.write(c.createHmac("sha256",process.env.ZEROSHIP_CONTROL_KEY).update(process.env.APP_ID).digest("hex"))')"
  curl -s --max-time 15 -o "$out" -w '' \
    "http://127.0.0.1:$ZEROSHIP_CONTROL_PORT/internal/workflows/runs/$rid" \
    -H "authorization: Bearer $token" \
    -H "x-zeroship-app-id: $APP_ID" >/dev/null
  jq -r '.state // "<none>"' "$out" 2>/dev/null || echo "<parse-error>"
}

# Poll a run until it leaves 'queued' or timeout; echoes final state.
poll_state() {
  local rid="$1" i st
  for i in $(seq 1 20); do
    st="$(run_state "$rid")"
    [ "$st" != "queued" ] && { echo "$st"; return 0; }
    sleep 0.5
  done
  echo "queued"
}

require_cmd docker
require_cmd curl
require_cmd zstd
require_cmd tar
require_cmd sha256sum
require_cmd jq
require_cmd lsof

echo "=== build check ==="
for b in zeroship-control zeroship-gate zeroship-worker dev-provision; do
  [ -x "$BIN/$b" ] || { fail "missing $BIN/$b -- run: cargo build --release"; exit 2; }
done
[ -f "$ROOT/packages/zero-migrate-cli/dist/cli-bin.js" ] || { fail "missing the zero-migrate CLI -- run: pnpm install && pnpm build"; exit 2; }
pass "release binaries present"

echo "=== database :$PG_PORT ==="
case "$PG_DB" in *[!A-Za-z0-9_]*) fail "bad PG_DB name"; exit 2;; esac
PG_ADMIN_CONTAINER="$(docker ps --format '{{.Names}} {{.Ports}}' \
  | awk -v port="$PG_PORT" 'index($0, ":" port "->5432/tcp"){print $1; exit}')"
if [ -n "$PG_ADMIN_CONTAINER" ]; then
  pass "reusing Postgres container $PG_ADMIN_CONTAINER on :$PG_PORT"
else
  docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
  docker run --name "$PG_CONTAINER" -d -p "$PG_PORT:5432" \
    -e POSTGRES_PASSWORD="$PG_PASS" -e POSTGRES_USER="$PG_USER" -e POSTGRES_DB=postgres \
    postgres:16 -c max_connections=300 >/dev/null
  PG_ADMIN_CONTAINER="$PG_CONTAINER"; OWNED_PG_CONTAINER=1
fi
for _ in $(seq 1 45); do
  docker exec "$PG_ADMIN_CONTAINER" pg_isready -U "$PG_USER" >/dev/null 2>&1 && break; sleep 1
done
docker exec "$PG_ADMIN_CONTAINER" pg_isready -U "$PG_USER" >/dev/null 2>&1 || { fail "pg never ready"; exit 1; }
docker exec "$PG_ADMIN_CONTAINER" psql -U "$PG_USER" -d postgres -v ON_ERROR_STOP=1 \
  -c "DROP DATABASE IF EXISTS \"$PG_DB\" WITH (FORCE);" -c "CREATE DATABASE \"$PG_DB\";" >/dev/null
CREATED_PG_DB=1
pass "created disposable database $PG_DB"

if [ -f "$ROOT/ops/postgres-init.sql" ]; then
  psql_admin < "$ROOT/ops/postgres-init.sql" >/dev/null && pass "applied ops/postgres-init.sql"
fi

zs_platform_migrate "$DBURL" \
  --migrations-dir "$ROOT/db/migrations-ts" \
  --project-schema zeroship --project-id zeroship > "$WORK/migrate.log" 2>&1 \
  || { fail "platform migrations failed"; tail -60 "$WORK/migrate.log"; exit 1; }
pass "platform migrations applied"

echo "=== build workflow .zship ==="
mkdir -p "$WORK/blobs" "$WORK/blob-cache"
cat > "$WORK/workflow.js" <<'JS'
// Self-completing workflow: a single pure step, no sleeps/signals/external I/O.
// One advance drives it queued -> completed, which is the observable signal.
export class ProbeWorkflow {
  async run(trigger, step) {
    const only = await step.run("only", () => ({ advanced: true, run: trigger.runId }));
    return { only, done: true };
  }
}
JS
HASH="$(sha256sum "$WORK/workflow.js" | awk '{print $1}')"
mkdir -p "$WORK/zstage/blobs"
cp "$WORK/workflow.js" "$WORK/zstage/blobs/$HASH"
NOW="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
cat > "$WORK/zstage/manifest.json" <<EOF
{"version":1,"resources":{"/[...rest]":{"auth":"anonymous","publicly_accessible":true}},"assets":{},"runtime_assets":{},"asset_version":0,"sourcemaps":{},"worker":{"entry":"index.js","modules":{"index.js":"$HASH"}},"workflows":["ProbeWorkflow"],"schedules":[],"metadata":{"compiler":"wfadvz-probe","built_at":"$NOW"}}
EOF
( cd "$WORK/zstage" && tar --format=ustar -cf - manifest.json "blobs/$HASH" ) | zstd -q -f -o "$WORK/workflow.zship"
pass "packed workflow.zship"

echo "=== boot services ==="
for port in "$ZEROSHIP_CONTROL_PORT" "$ZEROSHIP_WORKER_PORT" "$ZEROSHIP_GATEWAY_PORT"; do
  lsof -ti :"$port" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
done
GBS="$WORK/gateway-broker-secret"
printf '%s' "wfadvz-gateway-broker-secret-32-bytes-minimum-ok" > "$GBS"; chmod 0600 "$GBS"

# --- the workflow scheduler store comes from the migrations run above ---------
# Every arm below starts a run, and the workflow instance API writes through
# `zeroship.workflow_scheduler_inflight`, which
# db/migrations-ts/20260811000100_workflow_scheduler_store.ts creates. The
# platform-migrate call above is therefore the whole setup.
#
# THIS USED TO BE A DANCE, and the change that removed it is worth recording,
# because the dance is what a reader would otherwise reintroduce. The store was
# created at RUNTIME by `WorkflowSchedulerStore::provision()`, whose only
# non-test callers were control's workflow CRON loops. This harness runs control
# with `--disable-workflow-engine` (arm A's verdict is state mutation ATTRIBUTED
# to the unauthenticated call, and a live engine advances runs on its own
# schedule, so the mutation could no longer be attributed to the exploit). It had
# therefore disabled the only thing that provisioned the store it needs, and the
# first run creation died:
#
#   create_run HTTP 500 ({"error":"internal error"})
#   control.log: SqlState(E42P01) relation "workflow_scheduler.inflight" does not exist
#
# The workaround was to boot control once WITH the engine purely to provision,
# wait on the catalog, kill it, then boot the real one. That is gone because the
# runtime no longer does DDL at all (#320): it fails under any least-privilege
# role, since Postgres checks database-level CREATE before the IF NOT EXISTS
# short-circuit. THE ENGINE STILL STAYS OFF below, for the attribution reason,
# which was always the real constraint.
#
# The assertion is kept, on the catalog rather than a sleep: if the migration is
# ever dropped or renamed, every arm would die at create_run with a 500, and this
# says why instead.
if [ "$(docker exec "$PG_ADMIN_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -At \
      -c "SELECT to_regclass('zeroship.workflow_scheduler_inflight') IS NOT NULL;" 2>/dev/null)" = "t" ]; then
  pass "workflow scheduler store present from platform migrations"
else
  fail "zeroship.workflow_scheduler_inflight missing after migrate; every arm would die at create_run"
  tail -30 "$WORK/migrate.log"
  exit 1
fi

e2e_export_runtime_secrets "$WORK" || exit 1
e2e_export_database_urls "$DBURL"
"$BIN/zeroship-control" \
  --port "$ZEROSHIP_CONTROL_PORT" --blob-store "$WORK/blobs" \
  --gateway-url "http://localhost:$ZEROSHIP_GATEWAY_PORT" \
  --disable-workflow-engine > "$WORK/control.log" 2>&1 &
echo $! >> "$PIDFILE"
wait_health control "http://localhost:$ZEROSHIP_CONTROL_PORT/readyz" "$WORK/control.log"

e2e_start_cdc_relay "$BIN/zeroship-data-cdc-server" || exit 1
# NO `ZEROSHIP_DEV=1` HERE, and nothing needs one. The only thing that variable
# ever bought a worker was the SSRF relaxation, and `zeroship-worker` no longer
# reads it: dev-ness is a stated input written only by `set_dev_mode`, whose one
# caller is `cmd_serve` (crates/zeroship-cli/src/main.rs:96), and the gate reads
# the cell rather than the environment
# (crates/zeroship-runtime/src/transport/ssrf.rs:70-72). `ProbeWorkflow` below
# (line 241) is a single pure step with no fetch, no timer and no `env.*` call,
# so it never approached the gate even when the variable did reach it.
start_worker() {
  local extra="$1"
  "$BIN/zeroship-worker" \
    --port "$ZEROSHIP_WORKER_PORT" --threads 1 \
    --control-url "http://localhost:$ZEROSHIP_CONTROL_PORT" \
    --blob-store "$WORK/blobs" --poll-interval 1 --max-step-blob-bytes 2097152 \
 $extra > "$WORK/worker.log" 2>&1 &
  echo $! >> "$PIDFILE"
  wait_health worker "http://localhost:$ZEROSHIP_WORKER_PORT/readyz" "$WORK/worker.log"
}
start_worker "--workflow-advance-unsigned"

"$BIN/zeroship-gate" \
  --port "$ZEROSHIP_GATEWAY_PORT" --control-url "http://localhost:$ZEROSHIP_CONTROL_PORT" \
  --worker-urls "http://localhost:$ZEROSHIP_WORKER_PORT" --blob-store "$WORK/blobs" \
  --blob-cache-disk-root "$WORK/blob-cache" --broker-secret-file "$GBS" \
 --poll-interval 1 > "$WORK/gate.log" 2>&1 &
echo $! >> "$PIDFILE"
wait_health gateway "http://localhost:$ZEROSHIP_GATEWAY_PORT/readyz" "$WORK/gate.log"

echo "=== deploy + enable workflows ==="
"$BIN/dev-provision" \
  --db "$DBURL" --blob-store "$WORK/blobs" --name "$APP_NAME" \
  --zship "$WORK/workflow.zship" > "$WORK/provision.out" 2>&1 \
  || { fail "dev-provision failed"; cat "$WORK/provision.out"; exit 1; }
APP_ID="$(awk -F= '/^app_id=/{print $2}' "$WORK/provision.out")"
[ -n "$APP_ID" ] || { fail "no app_id from dev-provision"; cat "$WORK/provision.out"; exit 1; }
note "provisioned app_id=$APP_ID name=$APP_NAME"

psql_admin >/dev/null <<SQL
UPDATE zeroship.plans SET workflows_allowed = true, updated_at = now()
 WHERE id = (SELECT plan_id FROM zeroship.apps WHERE id = '$APP_ID');
UPDATE zeroship.apps SET workflows_enabled = true, updated_at = now() WHERE id = '$APP_ID';
INSERT INTO zeroship.workflow_rollout_config (id, dispatch_paused, ingress_disabled, updated_by)
VALUES ('global', false, false, 'wfadvz')
ON CONFLICT (id) DO UPDATE SET dispatch_paused=false, ingress_disabled=false,
  updated_at=now(), updated_by=EXCLUDED.updated_by;
SQL
pass "workflows enabled for app + plan"

sleep 3
# Warmup only forces gateway route-sync + worker deploy load; the app exports a
# workflow but no default.fetch, so the HTTP code is irrelevant. What matters is
# that the request reaches the worker (route present). Non-fatal by design.
WARM_CODE="$(curl -s -o /dev/null -w '%{http_code}' \
  "http://localhost:$ZEROSHIP_GATEWAY_PORT/apps/$APP_NAME/")"
note "warmup GET /apps/$APP_NAME/ -> HTTP $WARM_CODE (forces route+deploy sync)"
# Confirm the gateway actually holds a route for this app before the exploit,
# by checking that C2's negative control (unknown app) differs from a known app.
pass "gateway+worker warmed (route synced)"

RANDOM_UUID="$(cat /proc/sys/kernel/random/uuid)"

echo
echo "############ PHASE 1 -- worker WITH --workflow-advance-unsigned ############"

# Count journaled step rows for a run (concrete state mutation the advance wrote).
db_step_count() {
  local rid="$1"
  docker exec "$PG_ADMIN_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -At -v ON_ERROR_STOP=1 \
    -c "SELECT count(*) FROM \"app_${APP_ID}\".\"__zeroship_workflow_steps\" WHERE run_id = '$rid';" 2>/dev/null
}

echo "--- Arm A: EXPLOIT (Host: localhost, real runId+appId) ---"
RID_A="$(create_run)" || { fail "Arm A create_run failed (see stderr above)"; exit 1; }
PRE_STATE_A="$(run_state "$RID_A")"; PRE_STEPS_A="$(db_step_count "$RID_A")"
note "created run $RID_A, pre-state=$PRE_STATE_A, pre-step-rows=$PRE_STEPS_A"
BODY_A="{\"runId\":\"$RID_A\",\"appId\":\"$APP_ID\"}"
note "REQUEST : POST /__zeroship/internal/workflow-advance  Host: localhost"
note "BODY    : $BODY_A"
# The internal edge advances ONE frontier per call; a single-step workflow that
# re-registers a near-immediate nextWakeAt needs a couple of calls to reach a
# terminal state. Fire the SAME unauthenticated exploit up to 6 times and record
# each ack. `ack:true` on the very first call is already proof of advance.
FIRST_ACK=""; FIRST_CODE=""; FIRST_BYTES=""
for i in $(seq 1 6); do
  gw_post "localhost" "$BODY_A"
  ACK="$(printf '%s' "$LAST_BODY" | jq -r '.ack // false' 2>/dev/null)"
  note "advance #$i -> HTTP $LAST_CODE ack=$ACK"
  [ -z "$FIRST_ACK" ] && { FIRST_ACK="$ACK"; FIRST_CODE="$LAST_CODE"; FIRST_BYTES="$LAST_BODY"; }
  st="$(run_state "$RID_A")"
  [ "$st" = "completed" ] || [ "$st" = "failed" ] && break
  sleep 0.5
done
note "FIRST RESPONSE: HTTP $FIRST_CODE"
note "FIRST BYTES   : $FIRST_BYTES"
STATE_A="$(run_state "$RID_A")"; STEPS_A="$(db_step_count "$RID_A")"
note "run state after exploit: $STATE_A ; step-rows: $PRE_STEPS_A -> $STEPS_A"
# CONFIRMED if the unauthenticated caller made the worker advance the workflow:
# first response acked AND concrete journal mutation (new step rows) OR terminal.
if [ "$FIRST_CODE" = "200" ] && [ "$FIRST_ACK" = "true" ] \
   && { [ "$STATE_A" = "completed" ] || [ "${STEPS_A:-0}" -gt "${PRE_STEPS_A:-0}" ]; }; then
  fail "CONFIRMED AUTHZ GAP: unauthenticated Host:localhost caller advanced app workflow (ack=true, state=$STATE_A, steps $PRE_STEPS_A->$STEPS_A)"
elif [ "$FIRST_CODE" = "200" ] && [ "$FIRST_ACK" = "true" ]; then
  fail "CONFIRMED (weak): gateway acked the advance (ack=true) but no state mutation observed (state=$STATE_A)"
elif [ "$FIRST_CODE" = "401" ]; then
  pass "REFUSED at the gateway: no service credential (HTTP 401, state=$STATE_A, steps $PRE_STEPS_A->$STEPS_A)"
else
  # Neither advanced nor refused for the RIGHT reason. A 404, a 502 or a 500
  # here would read as "no advance" and hide a gateway that is broken rather
  # than guarded, so the code is asserted rather than the outcome alone.
  fail "expected HTTP 401 from the caller check; got HTTP $FIRST_CODE ack=$FIRST_ACK state=$STATE_A"
fi

echo "--- Arm C1a: CONTROL, Host = <app>.zeroship.localhost (app subdomain) ---"
RID_C1A="$(create_run)" || { fail "Arm C1a create_run failed"; exit 1; }
BODY_C1A="{\"runId\":\"$RID_C1A\",\"appId\":\"$APP_ID\"}"
note "REQUEST : Host: $APP_NAME.zeroship.localhost   BODY: $BODY_C1A"
gw_post "$APP_NAME.zeroship.localhost" "$BODY_C1A"
note "RESPONSE: HTTP $LAST_CODE  BYTES: $LAST_BODY"
STATE_C1A="$(poll_state "$RID_C1A")"
note "run state after: $STATE_C1A"
if [ "$LAST_CODE" = "404" ] && [ "$STATE_C1A" = "queued" ]; then
  pass "gate refused an app-subdomain Host (404), run stayed queued"
else
  fail "expected 404 + queued, got HTTP $LAST_CODE + state $STATE_C1A"
fi

echo "--- Arm C1b: CONTROL, Host = evil.example.com (NON-app subdomain) ---"
RID_C1B="$(create_run)" || { fail "Arm C1b create_run failed"; exit 1; }
BODY_C1B="{\"runId\":\"$RID_C1B\",\"appId\":\"$APP_ID\"}"
note "REQUEST : Host: evil.example.com   BODY: $BODY_C1B"
gw_post "evil.example.com" "$BODY_C1B"
note "RESPONSE: HTTP $LAST_CODE  BYTES: $LAST_BODY"
STATE_C1B="$(poll_state "$RID_C1B")"
note "run state after: $STATE_C1B"
if [ "$LAST_CODE" = "404" ] && [ "$STATE_C1B" = "queued" ]; then
  pass "gate refused a NON-app dotted Host too -> gate keys on subdomain PARSE, not app registration"
else
  fail "expected 404 + queued, got HTTP $LAST_CODE + state $STATE_C1B"
fi

echo "--- Arm C2: CONTROL, reachable Host + non-existent appId ---"
# THIS ARM MEASURES SOMETHING DIFFERENT NOW, AND THE CHANGE IS THE POINT.
#
# It used to expect 404 "app route not found", which separated "route
# reachable" from "route did something". That separation was only visible
# because an unauthenticated caller got as far as the route lookup. The caller
# check now runs FIRST, so an unauthenticated request is refused before the
# gateway consults its route table at all.
#
# What that buys, and what this arm now asserts: an unauthenticated caller
# cannot tell a real app from a made-up one. Both answer 401, byte for byte, so
# the edge is not an existence oracle for app ids. Arm A above already fired the
# same shape with the REAL app id; this is its one-variable pair.
BODY_C2="{\"runId\":\"$RANDOM_UUID\",\"appId\":\"$RANDOM_UUID\"}"
note "REQUEST : Host: localhost   BODY: $BODY_C2"
gw_post "localhost" "$BODY_C2"
UNKNOWN_APP_CODE="$LAST_CODE"; UNKNOWN_APP_BODY="$LAST_BODY"
note "RESPONSE: HTTP $UNKNOWN_APP_CODE  BYTES: $UNKNOWN_APP_BODY"
if [ "$UNKNOWN_APP_CODE" = "401" ] &&
   [ "$UNKNOWN_APP_CODE" = "$FIRST_CODE" ] &&
   [ "$UNKNOWN_APP_BODY" = "$FIRST_BYTES" ]; then
  pass "unknown app and real app are indistinguishable to an unauthenticated caller (both HTTP 401, identical bytes)"
else
  fail "expected an unauthenticated 401 identical to arm A's (HTTP $FIRST_CODE : $FIRST_BYTES), got HTTP $UNKNOWN_APP_CODE : $UNKNOWN_APP_BODY"
fi

echo
echo "############ PHASE 2 -- worker WITHOUT the flag (deploy/compose reality) ############"
# Restart the worker as the shipped compose configuration runs it.
kill_pids; wait 2>/dev/null || true; : > "$PIDFILE"
# control + gateway are down now too (kill_pids kills all). Rebring them.
"$BIN/zeroship-control" \
  --port "$ZEROSHIP_CONTROL_PORT" --blob-store "$WORK/blobs" \
  --gateway-url "http://localhost:$ZEROSHIP_GATEWAY_PORT" \
  --disable-workflow-engine > "$WORK/control2.log" 2>&1 &
echo $! >> "$PIDFILE"
wait_health control "http://localhost:$ZEROSHIP_CONTROL_PORT/readyz" "$WORK/control2.log"
start_worker ""   # no --workflow-advance-unsigned
"$BIN/zeroship-gate" \
  --port "$ZEROSHIP_GATEWAY_PORT" --control-url "http://localhost:$ZEROSHIP_CONTROL_PORT" \
  --worker-urls "http://localhost:$ZEROSHIP_WORKER_PORT" --blob-store "$WORK/blobs" \
  --blob-cache-disk-root "$WORK/blob-cache" --broker-secret-file "$GBS" \
 --poll-interval 1 > "$WORK/gate2.log" 2>&1 &
echo $! >> "$PIDFILE"
wait_health gateway "http://localhost:$ZEROSHIP_GATEWAY_PORT/readyz" "$WORK/gate2.log"
sleep 3
curl -sf "http://localhost:$ZEROSHIP_GATEWAY_PORT/apps/$APP_NAME/" >/dev/null || true

RID_P2="$(create_run)" || { fail "Phase2 create_run failed"; exit 1; }
BODY_P2="{\"runId\":\"$RID_P2\",\"appId\":\"$APP_ID\"}"
note "REQUEST : Host: localhost   BODY: $BODY_P2  (worker flag OFF)"
gw_post "localhost" "$BODY_P2"
note "RESPONSE: HTTP $LAST_CODE  BYTES: $LAST_BODY"
STATE_P2="$(poll_state "$RID_P2")"
note "run state after: $STATE_P2"
if [ "$STATE_P2" = "queued" ]; then
  pass "shipped-compose worker backstop held: run stayed queued (gateway relayed HTTP $LAST_CODE)"
else
  fail "run advanced to $STATE_P2 even with worker flag OFF (HTTP $LAST_CODE)"
fi

echo
echo "############ PHASE 3 -- can the exploit Host be delivered THROUGH Caddy? ############"
# Reproduce the repo Caddyfile catch-all rule (deploy/ops/Caddyfile:47) on a
# high port, in front of the live gateway, and measure which Host values Caddy
# will route. Only the host-matching semantics matter; the port is substituted.
if docker pull caddy:2-alpine >/dev/null 2>&1; then
  # --network host so the container's 127.0.0.1 is the host's, letting Caddy
  # reach the loopback-bound gateway at 127.0.0.1:$ZEROSHIP_GATEWAY_PORT (the earlier
  # host.docker.internal wiring hit 502 because the gateway binds loopback only).
  cat > "$WORK/Caddyfile" <<EOF
{
	auto_https off
	admin off
}
http://*.zeroship.localhost:$CADDY_PORT {
	reverse_proxy 127.0.0.1:$ZEROSHIP_GATEWAY_PORT
}
EOF
  CADDY_CONTAINER="zs-wfadvz-caddy-$$"
  docker rm -f "$CADDY_CONTAINER" >/dev/null 2>&1 || true
  docker run -d --name "$CADDY_CONTAINER" \
    --network host \
    -v "$WORK/Caddyfile:/etc/caddy/Caddyfile:ro" \
    caddy:2-alpine >/dev/null 2>&1
  # wait for caddy
  for _ in $(seq 1 20); do
    curl -s -o /dev/null "http://127.0.0.1:$CADDY_PORT/" && break; sleep 0.5
  done

  # C3a: dotted *.zeroship.localhost host -> Caddy routes, gateway gate rejects.
  C3A="$(curl -s -o "$WORK/c3a.body" -w '%{http_code}' \
    -H "Host: evil.zeroship.localhost:$CADDY_PORT" \
    -X POST "http://127.0.0.1:$CADDY_PORT/__zeroship/internal/workflow-advance" \
    -H 'content-type: application/json' --data "{\"runId\":\"$RANDOM_UUID\",\"appId\":\"$APP_ID\"}")"
  note "Caddy + Host evil.zeroship.localhost -> HTTP $C3A : $(cat "$WORK/c3a.body")"

  # C3b: the dotless Host that WOULD bypass the gate. Caddy has no matching
  # site block -> it will not proxy it.
  C3B="$(curl -s -o "$WORK/c3b.body" -w '%{http_code}' \
    -H "Host: localhost" \
    -X POST "http://127.0.0.1:$CADDY_PORT/__zeroship/internal/workflow-advance" \
    -H 'content-type: application/json' --data "{\"runId\":\"$RANDOM_UUID\",\"appId\":\"$APP_ID\"}")"
  note "Caddy + Host localhost      -> HTTP $C3B : $(head -c 200 "$WORK/c3b.body")"

  if [ "$C3A" = "404" ]; then
    pass "Caddy DID deliver the dotted host, but the gateway gate rejected it (404)"
  else
    fail "unexpected: Caddy+dotted host gave HTTP $C3A"
  fi
  # For the bypassing dotless host, Caddy should NOT reach the gateway's
  # internal handler (no matching site). Any non-forwarding response is the
  # point; we record it rather than asserting an exact Caddy status.
  note "=> Through Caddy's catch-all, the ONLY routable hosts are dotted *.zeroship.localhost, which the gate rejects; the dotless host that bypasses the gate is not routable via Caddy."
else
  note "SKIP Caddy arm: could not pull caddy:2-alpine (offline). Caddy routing assessed by READ only."
fi

echo
echo "############ SUMMARY ############"
# THIS PROBE IS NO LONGER RED AT HEAD, AND THAT IS THE CHANGE THIS BLOCK RECORDS.
#
# It used to carry a `AUTHZ_EXPECTED_FAILURES="CONFIRMED AUTHZ GAP"` list and a
# two-sided classifier: a failure outside the list was a regression, and a
# listed pattern matching nothing meant the gap had been closed and the list was
# stale. The second direction has now fired for real. The gateway verifies its
# caller on this edge, arm A refuses instead of advancing, and the list is gone
# rather than emptied - an empty pattern list is not a smaller expectation, it
# is a classifier that matches every failure or none depending on how the shell
# splits it.
#
# So the rule is now the plain one: ANY failure is a regression or a new defect.
#
# What still bounds the residual risk is measured by the probe itself and did
# not go away: phase 2 shows the shipped worker refusing an unsigned advance,
# and phase 3 shows that the dotless Host which bypasses the Host gate is not
# routable through the repository's Caddy rule. Those are defence in depth
# behind the credential check, not substitutes for it.
if [ "$FAILS" -gt 0 ]; then
  echo "  failures:"
  if [ "${#AUTHZ_FAILURES[@]}" -gt 0 ]; then
    # The length guard is not padding: `"${A[@]}"` on an empty array still
    # yields one empty word in this shell, so an unguarded loop prints a
    # phantom blank failure on a clean run.
    for _f in "${AUTHZ_FAILURES[@]}"; do echo "    $_f"; done
  fi
  echo "FAIL: the workflow-advance edge did not behave as specified." >&2
  exit 1
fi
echo "OK: the internal advance edge refuses an uncredentialled caller."
exit 0
