#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Redeploy: does a second deploy actually replace the running app, or does a
# worker keep serving a stale isolate?
#
# Made unmissable by redeploying a DIFFERENT app under the same name. A stale
# isolate is then not a subtle content difference but a procedure that should
# no longer exist still answering:
#
#   deploy A (kv-dashboard) -> kv.visit answers, getMessages does not exist
#   deploy B (starter)      -> getMessages answers, kv.visit must NOT exist
#
# Proven load-bearing by deploying A twice instead of A then B, which is what a
# stale isolate looks like from outside: 8 passed 0 failed becomes 6 passed
# 2 failed, and the two that fail are exactly the post-redeploy assertions.
#
# dev-provision reuses an app by name (`--name` is documented "create or reuse",
# and the reuse path calls set_deploy_with_manifest), so no PAT is needed here.
#
# Prereqs (docs/runbooks/local-dev.md):
#   cargo build --release -p zeroship-control -p zeroship-worker -p zeroship-gateway -p zeroship --bins
#   cargo build --release -p zeroship-migrate-adapter --features platform-cli --bin zeroship-platform-migrate
#   docker (Postgres on :5440 as compose-postgres-1, plus an ephemeral Redis)
#   pnpm build in examples/kv-dashboard and examples/starter
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release"
WORK="$(mktemp -d)"

PG_CONTAINER="${PG_CONTAINER:-compose-postgres-1}"
PG_USER="${PG_USER:-postgres}"
PG_DB="${PG_DB:-zeroship_redeploy}"
DB_URL="${DATABASE_URL:-postgres://postgres:zeroship@localhost:5440/$PG_DB}"
# Distinct from golden_path.sh and e2e_dev_vs_deployed_kv.sh.
CONTROL_PORT="${CONTROL_PORT:-9393}"
WORKER_PORT="${WORKER_PORT:-8393}"
GATE_PORT="${GATE_PORT:-8303}"
REDIS_PORT="${REDIS_PORT:-6397}"
REDIS_CONTAINER="zs-redeploy-redis"
export ZEROSHIP_DEV_INSECURE=1
export WORKER_KEY="${WORKER_KEY:-redeploy-worker-key-0123456789abcdef}"
APP_NAME="redep"
ZSHIP_A="${ZSHIP_A:-$ROOT/examples/kv-dashboard/dist/app.zship}"
ZSHIP_B="${ZSHIP_B:-$ROOT/examples/starter/dist/app.zship}"

PASS=0; FAIL=0; PIDS=()
ok() { PASS=$((PASS+1)); echo "  ok   $1"; }
no() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }
cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  docker rm -f "$REDIS_CONTAINER" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

# Are both sides the same BUILD? dev runs vite, which spawns `zeroship serve`;
# the deployed side runs separate server binaries. A partial rebuild leaves
# them at different commits and this harness would report the version skew as
# a dev-vs-deployed divergence. That already happened once, in the storage
# walk, where a worker predating 3e7e5e387 produced an error that read exactly
# like an S3 list defect. See tests/lib/binary_freshness.sh.
# shellcheck source=lib/binary_freshness.sh
source "$ROOT/tests/lib/binary_freshness.sh"
zs_check_binary_freshness "$ROOT" "$BIN" \
  "crates/runtime/src crates/worker/src crates/gateway/src crates/control/src crates/bundle/src" \
  "zeroship zeroship-worker zeroship-gate zeroship-control dev-provision" \
  || { _zs_fresh_rc=$?; [ "$_zs_fresh_rc" -ne 0 ] && exit "$_zs_fresh_rc"; }

echo "=== redeploy replaces the running app ==="
for z in "$ZSHIP_A" "$ZSHIP_B"; do
  [ -f "$z" ] || { no "missing $z (run pnpm build in that example)"; exit 1; }
done

for p in $CONTROL_PORT $WORKER_PORT $GATE_PORT; do lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done
docker rm -f "$REDIS_CONTAINER" >/dev/null 2>&1 || true
docker run --name "$REDIS_CONTAINER" -d -p "$REDIS_PORT:6379" redis:7-alpine >/dev/null
for _ in $(seq 1 30); do docker exec "$REDIS_CONTAINER" redis-cli ping 2>/dev/null | grep -q PONG && break; sleep 1; done
docker exec "$REDIS_CONTAINER" redis-cli ping 2>/dev/null | grep -q PONG || { no "Redis never became ready"; exit 1; }

docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "DROP DATABASE IF EXISTS $PG_DB WITH (FORCE)" >/dev/null 2>&1 || true
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "CREATE DATABASE $PG_DB" >/dev/null 2>&1 || true
"$BIN/zeroship-platform-migrate" --database-url "$DB_URL" --migrations-dir "$ROOT/db/migrations-ts" \
  --project-schema zeroship --project-id zeroship > "$WORK/migrate.log" 2>&1 \
  || { no "platform migrations failed"; tail -20 "$WORK/migrate.log"; exit 1; }

"$BIN/zeroship-control" --port "$CONTROL_PORT" --db "$DB_URL" --blob-store "$WORK/bundles" \
  --control-key rd-ck --master-key rd-mk > "$WORK/control.log" 2>&1 & PIDS+=($!)
sleep 4
# env.kv is absent without --kv-url by design, and kv-dashboard needs it.
"$BIN/zeroship-worker" --port "$WORKER_PORT" --worker-threads 2 --control "http://localhost:$CONTROL_PORT" \
  --control-key rd-ck --blob-store "$WORK/bundles" --poll-interval 2 \
  --kv-url "redis://127.0.0.1:$REDIS_PORT" > "$WORK/worker.log" 2>&1 & PIDS+=($!)
sleep 3
openssl rand -base64 48 > "$WORK/gate-secret"; chmod 600 "$WORK/gate-secret"
"$BIN/zeroship-gate" --port "$GATE_PORT" --control "http://localhost:$CONTROL_PORT" --control-key rd-ck \
  --workers "http://localhost:$WORKER_PORT" --blob-store "$WORK/bundles" \
  --gateway-broker-secret-file "$WORK/gate-secret" --poll-interval 2 > "$WORK/gate.log" 2>&1 & PIDS+=($!)
sleep 4
curl -sf "http://localhost:$GATE_PORT/health" >/dev/null && ok "stack healthy" \
  || { no "stack did not come up"; tail -20 "$WORK/gate.log"; exit 1; }

rpc() {
  curl -sS -m 15 -X POST -H 'content-type: application/json' -H "X-Api-Key: $KEY" \
    "http://localhost:$GATE_PORT/apps/$APP_NAME/__zeroship/v1/$1" -d '{"json":{}}' 2>&1
}
# An unknown procedure is refused by the GATEWAY at routing off the manifest,
# never reaching the dispatcher. Until 2026-08-11 the gateway said that in its
# own envelope ("no resource matched") while dev said "Method not found", and
# this alternation existed to paper over the difference. The dispatcher leg of
# `tests/e2e_dev_vs_deployed_errors.sh` measured that gap (plus two more of the
# same class) and it is now closed: the gateway answers RPC-path rejections in
# the worker's envelope, so BOTH tiers say
# `{"message":"Method not found: <id>","name":"Error","code":"NOT_FOUND"}`.
# The alternation is kept only so a bisect across that fix still reads correctly;
# the left branch is the live one.
absent() { printf '%s' "$1" | grep -qE "Method not found|no resource matched"; }

echo "=== deploy A"
OUT=$("$BIN/dev-provision" --db "$DB_URL" --blob-store "$WORK/bundles" --name "$APP_NAME" --zship "$ZSHIP_A" 2>&1)
KEY=$(echo "$OUT" | awk -F= '$1=="api_key"{print $2}')
APPID=$(echo "$OUT" | awk -F= '$1=="app_id"{print $2}')
[ -n "$KEY" ] && ok "deployed A (app_id=$APPID)" || { no "provision A: $OUT"; exit 1; }
sleep 6
A_VISIT=$(rpc kv.visit); A_MSGS=$(rpc getMessages)
# Values are printed BEFORE they are asserted on. An assertion whose pattern is
# wrong then shows its own evidence: matching the dev-only "Method not found"
# here once reported a stale isolate against an app that was behaving correctly.
echo "    A kv.visit    -> $(printf '%s' "$A_VISIT" | head -c 90)"
echo "    A getMessages -> $(printf '%s' "$A_MSGS" | head -c 90)"
printf '%s' "$A_VISIT" | grep -q '"visits"' && ok "A: kv.visit answers" || no "A: kv.visit did not answer"
absent "$A_MSGS" && ok "A: getMessages absent, as expected" || no "A: getMessages unexpectedly present"

# --- scenario 22: does a redeploy cut requests that are already in flight? ---
#
# The creator ships a fix while end users are mid-request. Until now this
# harness redeployed against an idle stack, so nothing here had ever measured
# what a user receives across the swap. docs/pilot/e2e-scenarios.md row 22.
#
# WHY THE OBSERVABLE IS 5xx-OR-TRANSPORT AND NOT "non-200": A and B expose
# DISJOINT procedures on purpose (kv.visit vs getMessages), so once B is live a
# kv.visit request SHOULD be refused. Counting non-200s -- the natural thing to
# write -- would report "redeploy breaks in-flight requests" out of a perfectly
# healthy run. A dropped connection or a 5xx is wrong on EITHER bundle, so that
# is what is asserted. Every code seen is printed regardless, so a wave of 401s
# (see the api-key note below) shows itself instead of hiding inside "not 5xx".
KEY_A="$KEY"
TRAF="$WORK/traffic.tsv"; : > "$TRAF"
STOPFILE="$WORK/traffic.stop"; rm -f "$STOPFILE"
(
  while [ ! -f "$STOPFILE" ]; do
    _s=$(date +%s%3N)
    _c=$(curl -sS -m 15 -o /dev/null -w '%{http_code}' -X POST \
      -H 'content-type: application/json' -H "X-Api-Key: $KEY_A" \
      "http://localhost:$GATE_PORT/apps/$APP_NAME/__zeroship/v1/kv.visit" \
      -d '{"json":{}}' 2>/dev/null)
    _rc=$?
    _e=$(date +%s%3N)
    printf '%s\t%s\t%s\t%s\n' "$_s" "$_e" "${_c:-000}" "$_rc" >> "$TRAF"
  done
) & TRAFFIC_PID=$!

echo "=== redeploy B under the SAME name"
DEPLOY_T0=$(date +%s%3N)
OUT2=$("$BIN/dev-provision" --db "$DB_URL" --blob-store "$WORK/bundles" --name "$APP_NAME" --zship "$ZSHIP_B" 2>&1)
APPID2=$(echo "$OUT2" | awk -F= '$1=="app_id"{print $2}')
KEY=$(echo "$OUT2" | awk -F= '$1=="api_key"{print $2}')
[ -n "$APPID2" ] && ok "redeployed B (app_id=$APPID2)" || { no "provision B: $OUT2"; exit 1; }
[ "$APPID" = "$APPID2" ] && ok "same app id reused, so this is a redeploy not a create" \
  || no "app id changed $APPID -> $APPID2, which is a create not a redeploy"
sleep 8   # gateway route-sync poll + worker bundle pickup
# T1 is AFTER the propagation sleep on purpose: the swap does not happen at the
# dev-provision call, it happens when the gateway re-syncs routes and the worker
# picks the new bundle up. Ending the window at the CLI return would bound it to
# an interval in which nothing had swapped yet.
DEPLOY_T1=$(date +%s%3N)
touch "$STOPFILE"; wait "$TRAFFIC_PID" 2>/dev/null || true

TR_TOTAL=$(wc -l < "$TRAF" | tr -d ' ')
# Overlap = the request was OPEN at some point inside the deploy window.
TR_OVER=$(awk -F'\t' -v a="$DEPLOY_T0" -v b="$DEPLOY_T1" '$1<=b && $2>=a' "$TRAF" | wc -l | tr -d ' ')
TR_BAD=$(awk -F'\t' '$4!=0 || $3 ~ /^5/' "$TRAF" | wc -l | tr -d ' ')
echo "    traffic: $TR_TOTAL requests, $TR_OVER overlapping the ${DEPLOY_T0}..${DEPLOY_T1} window"
echo "    codes:   $(awk -F'\t' '{print $3}' "$TRAF" | sort | uniq -c | tr '\n' ' ')"
[ "$KEY_A" = "$KEY" ] || echo "    NOTE: the api key ROTATED on redeploy, so 401s below are the old key, not a fault"
# DISCRIMINATION GUARD, and the first version of it WAS VACUOUS -- kept as a
# warning because it looked like the careful thing to write. It asserted
# TR_OVER > 0, but the traffic loop starts just before DEPLOY_T0 and is stopped
# at DEPLOY_T1, so every request is inside the window BY CONSTRUCTION; the first
# run duly printed "579 requests, 579 overlapping" and could not have printed
# anything else. A guard whose predicate cannot be false is decoration.
#
# What actually proves the traffic SPANNED the swap is that it saw both bundles:
# 200s while A served kv.visit, then 404s once B is live and kv.visit is gone.
# All-200 means the swap never landed during traffic; all-404 means traffic
# began after it. Either way the clean 5xx result would be about one side only.
TR_200=$(awk -F'\t' '$3==200' "$TRAF" | wc -l | tr -d ' ')
TR_404=$(awk -F'\t' '$3==404' "$TRAF" | wc -l | tr -d ' ')
{ [ "${TR_200:-0}" -gt 0 ] && [ "${TR_404:-0}" -gt 0 ]; } \
  && ok "traffic spanned the swap: $TR_200 served by A, then $TR_404 refused by B" \
  || no "traffic did NOT span the swap (200s=$TR_200, 404s=$TR_404), so the 5xx result below is one-sided"
[ "${TR_BAD:-1}" -eq 0 ] && ok "no in-flight request saw a transport failure or 5xx across the redeploy" \
  || { no "$TR_BAD request(s) hit a transport failure or 5xx across the redeploy"; \
       awk -F'\t' '$4!=0 || $3 ~ /^5/' "$TRAF" | head -5; }

B_VISIT=$(rpc kv.visit); B_MSGS=$(rpc getMessages)
echo "    B kv.visit    -> $(printf '%s' "$B_VISIT" | head -c 90)"
echo "    B getMessages -> $(printf '%s' "$B_MSGS" | head -c 90)"
printf '%s' "$B_MSGS" | grep -q "Build locally" && ok "B: getMessages answers, the new bundle is live" \
  || no "B: getMessages did not answer, so the new bundle is not serving"
absent "$B_VISIT" && ok "B: kv.visit is gone, no stale isolate" \
  || no "B: kv.visit STILL ANSWERS after redeploy, a stale isolate is serving the old bundle"

echo ""
echo "  redeploy: $PASS passed, $FAIL failed"
# ANTI-HOLLOW GUARD, the peer of the one in e2e_dev_vs_deployed_auth.sh. The
# verdict below is `FAIL == 0`, which is also true of a run that asserted
# nothing: PASS=0/FAIL=0 prints "0 passed, 0 failed" and exits 0. Eight sibling
# harnesses carry a MIN_PASSED floor; this one and the auth comparison did not
# (measured 2026-08-11).
#
# A > 0 check, not a numeric floor, on purpose: a floor needs a count measured
# from a live four-service run, and inventing one would be worse than the gap.
#
# BOUNDED HONESTLY: no reachable path to zero was found here either. Every
# assertion is an inline `cmd && ok || no` pair, so one arm always fires, and
# every setup failure hard-exits 1. Defence-in-depth against a hole the
# arithmetic allows and the control flow does not.
if [ "$((PASS + FAIL))" -eq 0 ]; then
  echo "FAIL: reached the verdict having asserted NOTHING. A redeploy check that" >&2
  echo "      checked nothing is indistinguishable from one that passed, and this" >&2
  echo "      script would otherwise have exited 0 over it." >&2
  exit 1
fi
# MEASURED FLOOR, not derived and not guessed. A full green run on 2026-08-11
# against the compose Postgres on :5440 scored `redeploy: 8 passed, 0 failed`,
# which matches the count this file's own header recorded independently. Two
# agreeing measurements, so 8 is the number.
#
# THE OUTPUT SHOWS NINE `ok` LINES AND PASS IS 8. The extra one is
# tests/lib/binary_freshness.sh:125, which prints "ok both sides built after
# the newest source under <crate>" with a bare echo rather than through this
# script's ok(), so it is a precondition notice and not a counted assertion.
# Anyone deriving a floor by counting output lines gets 9 and a permanently
# red gate; count PASS, not `ok` lines.
#
# Why a floor ON TOP of the > 0 check above: > 0 catches only total collapse.
# A run that lost the four post-redeploy assertions - exactly the ones that
# make this harness worth having - would still report 4 passed, 0 failed and
# exit 0.
REDEPLOY_MIN_PASSED="${REDEPLOY_MIN_PASSED:-8}"
if [ "$PASS" -lt "$REDEPLOY_MIN_PASSED" ]; then
  echo "FAIL: only $PASS assertions passed, fewer than the $REDEPLOY_MIN_PASSED this" >&2
  echo "      harness expects. Assertions do not vanish by accident: either a check" >&2
  echo "      stopped firing or one was removed. If the removal was deliberate, lower" >&2
  echo "      REDEPLOY_MIN_PASSED in the same change and say why." >&2
  exit 1
fi
[ "$FAIL" -eq 0 ]
