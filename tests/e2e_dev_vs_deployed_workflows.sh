#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Dev vs deployed: durable workflows.
#
# Runs ONE identical sequence of workflow cases against `pnpm dev` and against
# the same app deployed behind the gateway, then diffs the RESULTS. Peer of
# tests/e2e_dev_vs_deployed_{kv,storage,stream}.sh; scenario 11 (workflows leg)
# in docs/pilot/e2e-scenarios.md.
#
# Why this is not covered by tests/e2e_durable_workflows.sh. That script drives
# the deployed workflow engine hard, but it builds its app from an inline
# heredoc and hand-packs the `.zship` -- including a `"workflows":[...]` array
# it writes itself. So it proves the ENGINE runs workflows and cannot see
# whether a workflow authored in a normal vite app ever reaches the engine.
# That seam is what this script tests, and on the first run it was broken on
# BOTH sides (see docs/pilot/e2e-scenarios.md, scenario 11 workflows leg).
#
# Cases probed, one per durable primitive that distinguishes workflows:
#   basic       start + step.run + step.sideEffect
#   sleep       step.sleep (run suspends, then resumes to completion)
#   signal      step.waitForSignal (run parks, is signalled, resumes)
#   child       step.call (child run's output journaled into the parent)
#   compensate  a compensable step, then a permanent failure -> rollback
#
# Prereqs (docs/runbooks/local-dev.md):
#   pnpm build
#   cargo build --release -p zeroship-control -p zeroship-worker -p zeroship-gateway -p zeroship --bins
#   cargo build --release -p zeroship-migrate-adapter --features platform-cli --bin zeroship-platform-migrate
#   docker (Postgres on :5440 as compose-postgres-1, plus an ephemeral Redis)
#   pnpm install --filter workflow-probe...
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release"
APP="$ROOT/examples/workflow-probe"
ZSHIP="$APP/dist/app.zship"
WORK="$(mktemp -d)"

PG_CONTAINER="${PG_CONTAINER:-compose-postgres-1}"
PG_USER="${PG_USER:-postgres}"
PG_DB="${PG_DB:-zeroship_devdeploy_wf}"
DB_URL="${DATABASE_URL:-postgres://postgres:zeroship@localhost:5440/$PG_DB}"
# Distinct from golden_path.sh (9390/8390/8300) and the kv/storage/stream
# harnesses so several can run at once.
CONTROL_PORT="${CONTROL_PORT:-9395}"
WORKER_PORT="${WORKER_PORT:-8395}"
GATE_PORT="${GATE_PORT:-8305}"
DEV_PORT="${DEV_PORT:-3051}"
REDIS_PORT="${REDIS_PORT:-6399}"
REDIS_CONTAINER="zs-devdeploy-wf-redis"
CONTROL_KEY="dd-wf-ck"; MASTER_KEY="dd-wf-mk"
export ZEROSHIP_DEV_INSECURE=1
export WORKER_KEY="${WORKER_KEY:-devdeploy-worker-key-0123456789abcd}"
APP_NAME="wfprobe"

PASS=0; FAIL=0; PIDS=()
pass() { PASS=$((PASS+1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }
cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  lsof -ti :"$DEV_PORT" 2>/dev/null | xargs -r kill 2>/dev/null || true
  docker rm -f "$REDIS_CONTAINER" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

# Are both sides the same BUILD? `pnpm dev` runs target/release/zeroship; the
# deployed side runs separate binaries. A partial rebuild leaves them at
# different commits and this harness reports the skew as a divergence. See
# tests/lib/binary_freshness.sh for the run that cost a debugging cycle.
# shellcheck source=lib/binary_freshness.sh
source "$ROOT/tests/lib/binary_freshness.sh"
zs_check_binary_freshness "$ROOT" "$BIN" \
  "crates/plugin-workflow/src crates/runtime/src crates/worker/src crates/control/src sdks/workflows/src sdks/bootstrap/src" \
  "zeroship zeroship-worker zeroship-gate zeroship-control dev-provision" \
  || { [ "$?" -eq 2 ] && exit 2; }

echo "=== dev vs deployed (workflow-probe) ==="

# --- the probe: one deterministic sequence, printed as RESULTS ---
#
# What is normalised, and why. Nothing is blanked by a regex here. Two fields
# ARE dropped, both on the server side in `wf.status`, and both because they are
# non-deterministic rather than inconvenient:
#   runId  -- a fresh id per run, never equal across two runs let alone two
#             backends. It is returned by `wf.start` and used to poll, so the
#             harness exercises it; it just is not part of the compared output.
#   error.stack -- the deployed envelope names a per-process module id
#             (`__zs_kind_bridge_<uuid>.js`) and minified line numbers.
# And one thing is CANONICALISED rather than dropped: object key ORDER. A step
# output authored as `{label, n}` comes back `{label, n}` from dev and `{n,
# label}` from the deployed app, because the deployed journal round-trips
# through Postgres `jsonb`, which stores keys sorted by length then bytes, while
# the dev engine keeps the JSON text SQLite was handed. That is a real
# difference and it is recorded as a finding, but JSON object key order is not
# part of any contract here, so comparing it verbatim would make this script
# permanently red on something no creator can rely on either way. `canon` below
# sorts keys on BOTH sides; delete it and the diff shows the raw ordering.
# Everything else is compared verbatim: run states, step outputs, error type and
# message, the `compensation` rollback summary, and the kv compensation trail --
# across SQLite/redb (dev) and Postgres/Redis (deployed).
#
# The poll loop lives HERE, not inside an RPC. An earlier version put it in a
# `wf.run` mutation so both sides ran byte-identical logic; that version passed
# in dev and returned `{"message":"request timed out"}` for every case deployed,
# because `zeroship serve` leaves the per-request wall clock unbounded
# (crates/runtime/src/core/serve.rs, `wall_timeout: None`) and a deployed app
# inherits FREE_TIER_RUNTIME_LIMITS -- 5s wall (crates/core/src/types.rs:37).
# The loop is identical for both URLs, so the two sides still see the same
# sequence of operations at the same cadence.
probe() {
  local url="$1" hdr="${2:-}" rpc="$1/__zeroship/v1"
  local h=(); [ -n "$hdr" ] && h=(-H "$hdr")
  call() {
    curl -sS -m 20 -X POST -H 'content-type: application/json' "${h[@]}" \
      "$rpc/$1" -d "{\"json\":${2:-{\}}}" 2>&1
  }
  # One case: start, then poll status once a second until terminal. The `signal`
  # case is signalled the first time the run reports `waiting` -- signalling
  # before the run parks would be a different test, and doing it on a fixed
  # timer would make the two sides diverge on scheduler speed rather than on
  # behaviour.
  run_case() {
    local case="$1" wf="$2" started rid state st signalled=0 i
    started=$(call wf.start "{\"case\":\"$case\"}")
    rid=$(printf '%s' "$started" | grep -oE 'run_[A-Za-z0-9]+' | head -1)
    if [ -z "$rid" ]; then
      echo "START-REFUSED $started"
      return
    fi
    st=""
    for i in $(seq 1 60); do
      st=$(call wf.status "{\"workflow\":\"$wf\",\"runId\":\"$rid\"}")
      state=$(printf '%s' "$st" | sed -nE 's/.*"state":"([a-z-]+)".*/\1/p')
      if [ "$case" = "signal" ] && [ "$signalled" -eq 0 ] && [ "$state" = "waiting" ]; then
        call wf.signal "{\"workflow\":\"$wf\",\"runId\":\"$rid\",\"token\":\"probe-token\"}" >/dev/null
        signalled=1
      fi
      case "$state" in completed|failed|cancelled) break ;; esac
      sleep 1
    done
    echo "$st"
  }
  echo "ping       $(call wf.ping)"
  call wf.resetTrail >/dev/null
  echo "basic      $(run_case basic BasicCase)"
  echo "sleep      $(run_case sleep SleepCase)"
  echo "signal     $(run_case signal SignalCase)"
  echo "child      $(run_case child ChildCase)"
  echo "compensate $(run_case compensate CompensateCase)"
  echo "trail      $(call wf.trail)"
}

# --- 1. real build ---
( cd "$APP" && pnpm build ) > "$WORK/build.log" 2>&1
[ -f "$ZSHIP" ] && pass "built app.zship ($(du -k "$ZSHIP" | cut -f1)KB)" \
  || { fail "build produced no app.zship"; tail -20 "$WORK/build.log"; exit 1; }

d="$WORK/unpack"; mkdir -p "$d"; tar -xf "$ZSHIP" -C "$d"

# Every RPC resource must declare an auth posture, or it resolves to
# `auth: user` and the gateway refuses it -- green in dev, 401 deployed.
total=$(grep -oE '"rpc:[^"]+":' "$d/manifest.json" | wc -l)
authed=$(grep -oE '"rpc:[^"]+":\{[^}]*"auth":' "$d/manifest.json" | wc -l)
[ "$total" -gt 0 ] && [ "$authed" -eq "$total" ] \
  && pass "all $total rpc resources declare an auth posture" \
  || fail "only $authed of $total rpc resources declare auth (missing src/server/config.ts?)"

# The manifest must DECLARE every workflow the app exports. The control plane
# refuses `start` for a name the active deploy does not declare
# (crates/control/src/workflow_instance_api.rs:732), so an app whose build
# drops the declaration is deployable, reachable, and cannot run a workflow.
# This assertion is the build-side half of the same failure the results diff
# catches at run time; keeping both means a regression is attributed to the
# packer rather than looking like a control-plane bug.
for wf in BasicCase SleepCase SignalCase ChildCase CompensateCase DoubleChild; do
  grep -q "\"$wf\"" "$d/manifest.json" \
    && pass "manifest declares workflow $wf" \
    || fail "manifest does NOT declare workflow $wf (built .zship cannot run it)"
done

# --- 2. dev side ---
lsof -ti :"$DEV_PORT" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
# A stale dev journal would let a previous run's rows resolve and mask a
# regression, so start from an empty local state directory.
rm -rf "$APP/.zeroship"
( cd "$APP" && ./node_modules/.bin/vite > "$WORK/dev.log" 2>&1 ) & PIDS+=($!)
for _ in $(seq 1 25); do
  curl -sf -o /dev/null -m 2 -X POST -H 'content-type: application/json' \
    "http://localhost:$DEV_PORT/__zeroship/v1/wf.ping" -d '{"json":{}}' && break
  sleep 2
done
probe "http://localhost:$DEV_PORT" > "$WORK/dev.txt" 2>&1
grep -q '"ok":true' "$WORK/dev.txt" && pass "dev server answered the probe" \
  || { fail "dev server never answered"; tail -20 "$WORK/dev.log"; exit 1; }

# --- 3. deployed side ---
docker rm -f "$REDIS_CONTAINER" >/dev/null 2>&1 || true
docker run --name "$REDIS_CONTAINER" -d -p "$REDIS_PORT:6379" redis:7-alpine >/dev/null
for _ in $(seq 1 30); do docker exec "$REDIS_CONTAINER" redis-cli ping 2>/dev/null | grep -q PONG && break; sleep 1; done
docker exec "$REDIS_CONTAINER" redis-cli ping 2>/dev/null | grep -q PONG \
  && pass "ephemeral Redis ready on :$REDIS_PORT" || { fail "Redis never became ready"; exit 1; }

for p in $CONTROL_PORT $WORKER_PORT $GATE_PORT; do lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "DROP DATABASE IF EXISTS $PG_DB WITH (FORCE)" >/dev/null 2>&1 || true
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "CREATE DATABASE $PG_DB" >/dev/null 2>&1 || true
"$BIN/zeroship-platform-migrate" --database-url "$DB_URL" --migrations-dir "$ROOT/db/migrations-ts" \
  --project-schema zeroship --project-id zeroship > "$WORK/migrate.log" 2>&1 \
  || { fail "platform migrations failed"; tail -20 "$WORK/migrate.log"; exit 1; }

# --gateway-url is the seam the control workflow engine dispatches a run
# through. Its default is `http://localhost` (port 80), so without this every
# run is created, claimed, and never advances -- the RPC poll loop then hits
# the worker's request timeout and the failure reads like a workflow-engine
# bug. Unlike tests/e2e_durable_workflows.sh, this harness must let control's
# own scheduler run (that script passes --disable-workflow-engine and drives
# the engine from a Rust test): the whole point here is the path a deployed
# app actually takes.
"$BIN/zeroship-control" --port "$CONTROL_PORT" --db "$DB_URL" --blob-store "$WORK/bundles" \
  --control-key "$CONTROL_KEY" --master-key "$MASTER_KEY" \
  --gateway-url "http://localhost:$GATE_PORT" > "$WORK/control.log" 2>&1 & PIDS+=($!)
sleep 4
# --control-url + --control-key give the worker's env.workflows namespace its
# HTTP backend; --kv-url gives env.kv one. Omitting either would look like an
# app bug rather than a harness one.
#
# --workflow-advance-unsigned is not a shortcut here; it is the ONLY way a
# deployed run can advance today. The control workflow engine POSTs to the
# gateway's `/__zeroship/internal/workflow-advance`
# (crates/control/src/cron/workflow_engine.rs:209), the gateway forwards to the
# worker's `/workflow-advance-unsigned/{app_id}`
# (crates/gateway/src/proxy.rs:257), and that worker handler returns 403 unless
# the flag is set (crates/worker/src/handler.rs:553). The handler's own doc says
# signed advance "replaces it in a later durable-workflows task" and that
# `deploy/` passes the flag nowhere -- confirmed: the only two matches in the
# tree are this file's sibling e2e and its worktree copy. So a production
# deployment as shipped in deploy/compose cannot advance a workflow at all.
# Recorded as a finding in docs/pilot/e2e-scenarios.md rather than worked around.
"$BIN/zeroship-worker" --port "$WORKER_PORT" --worker-threads 2 --control "http://localhost:$CONTROL_PORT" \
  --control-key "$CONTROL_KEY" --blob-store "$WORK/bundles" --poll-interval 2 \
  --db "$DB_URL" --workflow-advance-unsigned \
  --kv-url "redis://127.0.0.1:$REDIS_PORT" > "$WORK/worker.log" 2>&1 & PIDS+=($!)
sleep 3
openssl rand -base64 48 > "$WORK/gate-secret"; chmod 600 "$WORK/gate-secret"
"$BIN/zeroship-gate" --port "$GATE_PORT" --control "http://localhost:$CONTROL_PORT" \
  --control-key "$CONTROL_KEY" --workers "http://localhost:$WORKER_PORT" --blob-store "$WORK/bundles" \
  --gateway-broker-secret-file "$WORK/gate-secret" --poll-interval 2 \
  --db "$DB_URL" > "$WORK/gate.log" 2>&1 & PIDS+=($!)
sleep 4
for svc in "control:$CONTROL_PORT" "worker:$WORKER_PORT" "gateway:$GATE_PORT"; do
  curl -sf "http://localhost:${svc##*:}/health" >/dev/null \
    || { fail "${svc%%:*} did not come up"; tail -20 "$WORK/${svc%%:*}.log"; exit 1; }
done
pass "control + worker + gateway healthy"

OUT=$("$BIN/dev-provision" --db "$DB_URL" --blob-store "$WORK/bundles" --name "$APP_NAME" --zship "$ZSHIP" 2>&1)
API_KEY=$(echo "$OUT" | awk -F= '$1 == "api_key" { print $2 }')
APP_ID=$(echo "$OUT" | awk -F= '$1 == "app_id" { print $2 }')
[ -n "$API_KEY" ] && [ -n "$APP_ID" ] && pass "deployed $APP_NAME" || { fail "provision: $OUT"; exit 1; }

# Durable workflows are behind an operator rollout gate, not a creator switch:
# `apps.workflows_enabled` AND `plans.workflows_allowed` must both be true
# (crates/control/src/workflow_rollout.rs:12-34) or every start returns 403
# "workflows are not enabled for this app or plan". `dev-provision` sets
# neither, and nothing on the creator path can. Flipping them here is
# provisioning, not a weakened assertion -- tests/e2e_durable_workflows.sh does
# the same at its line 621. The dev engine has no equivalent gate, which is a
# real dev-vs-deployed difference recorded in docs/pilot/e2e-scenarios.md.
docker exec -i "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -v ON_ERROR_STOP=1 >/dev/null <<SQL
UPDATE zeroship.plans SET workflows_allowed = true, updated_at = now()
 WHERE id = (SELECT plan_id FROM zeroship.apps WHERE id = '$APP_ID');
UPDATE zeroship.apps SET workflows_enabled = true, updated_at = now()
 WHERE id = '$APP_ID';
INSERT INTO zeroship.workflow_rollout_config (id, dispatch_paused, ingress_disabled, updated_by)
VALUES ('global', false, false, 'devdeploy-wf')
ON CONFLICT (id) DO UPDATE SET dispatch_paused = false, ingress_disabled = false,
  updated_at = now(), updated_by = EXCLUDED.updated_by;
SQL
pass "workflows enabled for $APP_NAME (operator gate, not creator-settable)"
sleep 5   # gateway route-sync poll

probe "http://localhost:$GATE_PORT/apps/$APP_NAME" "X-Api-Key: $API_KEY" > "$WORK/deployed.txt" 2>&1
grep -q '"ok":true' "$WORK/deployed.txt" && pass "deployed app answered the probe" \
  || { fail "deployed app never answered"; head -5 "$WORK/deployed.txt"; tail -5 "$WORK/worker.log"; }

# Re-emit each `<label> <json>` line with object keys sorted, so the comparison
# is on values rather than on Postgres jsonb's key ordering. A line whose tail
# is not JSON (a transport error, say) is passed through untouched -- swallowing
# it would turn a failure into a silent match.
canon() {
  node -e '
    const fs = require("fs");
    const sortKeys = (v) => Array.isArray(v)
      ? v.map(sortKeys)
      : (v && typeof v === "object"
          ? Object.fromEntries(Object.keys(v).sort().map((k) => [k, sortKeys(v[k])]))
          : v);
    for (const line of fs.readFileSync(process.argv[1], "utf8").split("\n")) {
      if (line === "") continue;
      const cut = line.indexOf(" ");
      const label = cut < 0 ? line : line.slice(0, cut);
      const rest = cut < 0 ? "" : line.slice(cut + 1).trim();
      try {
        console.log(label + " " + JSON.stringify(sortKeys(JSON.parse(rest))));
      } catch {
        console.log(line);
      }
    }
  ' "$1"
}

line_for() { grep -E "^$2 " "$WORK/$1.txt" | head -1; }

# Both sides must actually COMPLETE, not merely agree. Without this a platform
# that failed every run identically would diff clean and the comparison would
# certify a broken product as consistent.
for side in dev deployed; do
  for c in basic sleep signal; do
    printf '%s' "$(line_for "$side" "$c")" | grep -q '"state":"completed"' \
      && pass "$side: $c reached state=completed" \
      || fail "$side: $c did NOT complete -- $(line_for "$side" "$c" | head -c 300)"
  done
done

# --- 4. THE POINT: identical operations must produce identical results ---
#
# Compared in two groups, because two of the five cases are KNOWN to diverge and
# lumping them in would either hide the three that must match or make the script
# permanently red. The known pair is pinned to its exact current behaviour on
# BOTH sides, so this still fails if either side changes -- including when the
# dev tier grows the missing feature, which is the point at which the pin should
# be deleted rather than edited.
KNOWN_DIVERGENT='^(child|compensate|trail) '
canon "$WORK/dev.txt"      | grep -vE "$KNOWN_DIVERGENT" > "$WORK/dev.cmp"
canon "$WORK/deployed.txt" | grep -vE "$KNOWN_DIVERGENT" > "$WORK/deployed.cmp"

if diff -q "$WORK/dev.cmp" "$WORK/deployed.cmp" >/dev/null 2>&1; then
  pass "dev and deployed are identical on start/step.run/sideEffect, sleep and signal"
else
  fail "dev and deployed DIVERGE on a case expected to match (< dev, > deployed)"
  diff "$WORK/dev.cmp" "$WORK/deployed.cmp" | head -60
  echo ""
  echo "  A divergence here is the finding, not a flaky test. See"
  echo "  docs/pilot/e2e-scenarios.md before weakening anything above."
fi

# Known divergence 1 -- child workflows. The dev mini-engine refuses every
# `child` checkpoint outright (crates/plugin-workflow/src/dev.rs
# reject_child_checkpoints_for_dev), so `step.call` cannot work locally at all.
# Deployed it runs and journals the child's output into the parent.
printf '%s' "$(line_for dev child)" \
  | grep -q '"type":"WorkflowUnsupportedError"' \
  && pass "known divergence: dev refuses step.call (WorkflowUnsupportedError)" \
  || fail "dev child behaviour CHANGED -- $(line_for dev child | head -c 300)"
printf '%s' "$(line_for deployed child)" \
  | grep -q '"parentSaw":42' \
  && pass "deployed: step.call completed and journaled the child output" \
  || fail "deployed child did not complete -- $(line_for deployed child | head -c 300)"

# Known divergence 2 -- compensation. The dev engine has no compensating phase:
# its RunUpdate enum stops at Failed/Stalled/Cancelled (dev.rs:208-217) and
# nothing ever writes state='compensating', so a compensable step that fails is
# simply left un-rolled-back, with no error and no warning. Deployed, the
# compensator runs and the error envelope carries a rollback summary. The trail
# is the observable: `do:reserve` alone means the undo never happened.
printf '%s' "$(line_for dev compensate)" | grep -q '"state":"failed"' \
  && pass "dev: compensable run reached state=failed" \
  || fail "dev compensate state CHANGED -- $(line_for dev compensate | head -c 300)"
printf '%s' "$(line_for dev trail)" | grep -q '"trail":"do:reserve"' \
  && pass "known divergence: dev ran NO compensator (trail=do:reserve)" \
  || fail "dev compensation behaviour CHANGED -- $(line_for dev trail | head -c 300)"
printf '%s' "$(line_for deployed trail)" | grep -q '"trail":"do:reserve,undo:reserve"' \
  && pass "deployed: compensator ran (trail=do:reserve,undo:reserve)" \
  || fail "deployed compensator did NOT run -- $(line_for deployed trail | head -c 300)"
printf '%s' "$(line_for deployed compensate)" \
  | grep -q '"outcome":"completed"' \
  && pass "deployed: error envelope reports compensation outcome=completed" \
  || fail "deployed compensation summary missing -- $(line_for deployed compensate | head -c 300)"

echo ""
echo "  --- dev results ---"; cat "$WORK/dev.txt"
echo "  --- deployed results ---"; cat "$WORK/deployed.txt"
echo ""
echo "  dev vs deployed (workflows): $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
