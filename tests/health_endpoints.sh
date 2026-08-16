#!/usr/bin/env bash
# ============================================================================
# health_endpoints.sh - the /healthz + /readyz contract, on the REAL binaries.
#
# Every platform service exposes the SAME pair:
#
#   /healthz   liveness. Constant 200. Touches no dependency, so a Postgres
#              blip cannot get the container killed for someone else's outage.
#   /readyz    readiness. 200 only when the dependencies the service cannot
#              work without are answering:
#                control   Postgres
#                migrated  Postgres
#                auth      Postgres
#                worker    control poll current AND blob store reachable
#                gateway   control route pull current
#
# What this walks, against a CLEAN ephemeral stack:
#   1. control + worker + gateway (tests/lib/e2e_stack.sh), plus migrated and
#      auth booted here against the same database.
#   2. For all FIVE: /healthz == 200 and /readyz == 200.
#   3. For all FIVE: the old /health is GONE (404). Pre-launch, no alias -
#      an added endpoint that does not delete the old one is half a rename,
#      and only this arm can tell the two apart.
#   4. THE DOWN CASES. Adding a second endpoint that is always 200 is not
#      readiness; these are the arms that make it real. Each pairs a failing
#      service with the SAME service's /healthz, so a 503 cannot be confused
#      with a process that never started:
#        a. a gateway whose --control-url points at a closed port -> /healthz
#           200, /readyz 503 (freshness arm, "never fresh")
#        b. Postgres STOPPED under the live stack -> control, migrated and
#           auth answer /healthz 200 and /readyz 503 (actively-probed arm,
#           against a real outage rather than a mis-pointed flag)
#        c. worker and gateway then age out to 503 on their own, because
#           control can no longer serve /internal/{versions,routes}
#           (freshness arm, "was fresh, then aged out")
#
# What this does NOT catch:
#   - Recovery. Nothing here brings a dependency BACK to prove /readyz returns
#     to 200; that is a timing test against the cache TTL.
#   - The single-flight bound on probe fan-out. That is unit-tested in
#     crates/core/src/readiness.rs, not here - a shell loop cannot observe how
#     many Postgres round trips one probe burst caused.
#   - The worker's blob-store arm failing on its own. Case (c) takes the
#     worker's CONTROL dependency away, not its blob store; the blob-store
#     probe's failing arm is covered by the LocalDiskBlobStore unit tests in
#     crates/bundle/src/blob.rs.
#   - Whether a 503 body leaks anything. The assertions read status codes
#     only; the no-detail-leak property is a code review of the handlers.
#
# Usage:
#   ./tests/health_endpoints.sh
#
# Requires: docker, openssl, curl, lsof; a release build of zeroship-control,
#   zeroship-gate, zeroship-worker, zeroship-auth, zeroship-migrated and
#   zeroship-platform-migrate.
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release"

# Private port band, chosen not to collide with the other harnesses.
export ZEROSHIP_CONTROL_PORT=9126
export ZEROSHIP_WORKER_PORT=8102
export ZEROSHIP_GATEWAY_PORT=8016
export PG_PORT=5458
export PG_CONTAINER="zs-e2e-health-pg"
MIGRATED_PORT=9131
AUTH_PORT=9134
GATEWAY_DOWN_PORT=8017
# A port nothing listens on. Both down cases point a dependency here.
DEAD_PORT=9199

PASS=0; FAIL=0
EXTRA_PIDS=()

pass() { PASS=$((PASS+1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }

# shellcheck source=tests/lib/e2e_stack.sh
source "$ROOT/tests/lib/e2e_stack.sh"

cleanup() {
  echo ""
  echo "=== Cleanup ==="
  for pid in "${EXTRA_PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
  stack_down
  echo "  stack down, ephemeral PG removed"
}
trap cleanup EXIT

# HTTP status of a GET, or 000 when nothing answered. `--max-time` is curl's
# OWN deadline on purpose: an external `timeout` SIGKILLs before curl flushes,
# so a real response reads as silence.
code() { curl -s -o /dev/null -w '%{http_code}' --max-time 5 "$1" 2>/dev/null; }

# The whole contract for one service, in its healthy state.
#
# The third arm does NOT assert a literal 404. Four of the five services answer
# an unrouted path with 404, but the gateway hands anything it does not
# recognise to its subdomain catch-all, which answers 400 for a bare
# `localhost` Host. Hardcoding 404 measured that quirk rather than the deletion
# and failed on the gateway alone. Comparing `/health` against a path that was
# NEVER registered asserts the thing that actually matters - `/health` is no
# longer special-cased - and says it identically for all five.
assert_healthy_pair() {
  local name="$1" base="$2" c gone unknown
  c="$(code "$base/healthz")"
  [ "$c" = "200" ] && pass "$name /healthz 200" || fail "$name /healthz expected 200, got $c"
  c="$(code "$base/readyz")"
  [ "$c" = "200" ] && pass "$name /readyz 200" || fail "$name /readyz expected 200, got $c"
  gone="$(code "$base/health")"
  unknown="$(code "$base/zz-never-a-route")"
  if [ "$gone" != "200" ] && [ "$gone" = "$unknown" ]; then
    pass "$name /health is gone (answers $gone, same as an unregistered path)"
  else
    fail "$name /health should answer like an unregistered path; got $gone vs $unknown"
  fi
}

echo "=== Build check ==="
for b in zeroship-control zeroship-gate zeroship-worker zeroship-auth zeroship-migrated \
         zeroship-platform-migrate; do
  [ -x "$BIN/$b" ] || { fail "missing $BIN/$b - run: cargo build --release"; exit 2; }
done
pass "all five service binaries present"

echo ""
echo "=== Stack: control + worker + gateway ==="
# stack_workspace + stack_pg_up rather than stack_up: stack_up's preflight
# requires node and the workspace `jose` because it can mint PATs, and nothing
# in this harness authenticates anything. Booting the three binaries here keeps
# the health contract testable on a checkout that has never run `pnpm install`.
stack_workspace || { fail "stack_workspace failed"; exit 1; }
stack_pg_up || { fail "stack_pg_up failed"; exit 1; }

for p in $ZEROSHIP_CONTROL_PORT $ZEROSHIP_WORKER_PORT $ZEROSHIP_GATEWAY_PORT \
         $MIGRATED_PORT $AUTH_PORT $GATEWAY_DOWN_PORT; do
  lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
done

e2e_with_platform_mint_key "$BIN/zeroship-control" --port "$ZEROSHIP_CONTROL_PORT" \
  --blob-store "$WORK/blobs" --signing-key-file "$WORK/signing-key.pem" \
  > "$WORK/control.log" 2>&1 &
EXTRA_PIDS+=($!)
for _ in $(seq 1 30); do
  [ "$(code "http://localhost:$ZEROSHIP_CONTROL_PORT/readyz")" = "200" ] && break; sleep 1
done

"$BIN/zeroship-worker" --port "$ZEROSHIP_WORKER_PORT" --threads 2 \
  --control-url "http://localhost:$ZEROSHIP_CONTROL_PORT" \
  --blob-store "$WORK/blobs" --poll-interval 2 > "$WORK/worker.log" 2>&1 &
EXTRA_PIDS+=($!)
for _ in $(seq 1 30); do
  [ "$(code "http://localhost:$ZEROSHIP_WORKER_PORT/readyz")" = "200" ] && break; sleep 1
done

"$BIN/zeroship-gate" --port "$ZEROSHIP_GATEWAY_PORT" \
  --control-url "http://localhost:$ZEROSHIP_CONTROL_PORT" \
  --worker-urls "http://localhost:$ZEROSHIP_WORKER_PORT" --blob-store "$WORK/blobs" \
  --blob-cache-disk-root "$WORK/blob-cache" --poll-interval 2 \
  --signing-key-file "$WORK/signing-key.pem" \
  --broker-secret-file "$WORK/gate-secret" \
  > "$WORK/gate.log" 2>&1 &
EXTRA_PIDS+=($!)
for _ in $(seq 1 30); do
  [ "$(code "http://localhost:$ZEROSHIP_GATEWAY_PORT/readyz")" = "200" ] && break; sleep 1
done

echo ""
echo "=== Stack: migrated + auth ==="
"$BIN/zeroship-migrated" --port "$MIGRATED_PORT" \
  --signing-key-file "$WORK/signing-key.pem" --tmp-dir "$WORK/migrated-tmp" \
  > "$WORK/migrated.log" 2>&1 &
EXTRA_PIDS+=($!)
for _ in $(seq 1 30); do
  [ "$(code "http://localhost:$MIGRATED_PORT/readyz")" = "200" ] && break; sleep 1
done

AUTH_URL="http://localhost:$AUTH_PORT"
e2e_with_platform_mint_key "$BIN/zeroship-auth" \
  --addr "127.0.0.1:$AUTH_PORT" --public-url "$AUTH_URL" \
  --signing-key-file "$ZEROSHIP_AUTH_SIGNING_KEY_FILE" \
  --pairwise-salt-file "$ZEROSHIP_AUTH_PAIRWISE_SALT_FILE" \
  --broker-secret-file "$ZEROSHIP_AUTH_BROKER_SECRET_FILE" \
  --refresh-hash-key-file "$ZEROSHIP_AUTH_REFRESH_HASH_KEY_FILE" \
  --refresh-idem-key-file "$ZEROSHIP_AUTH_REFRESH_IDEM_KEY_FILE" \
  --mailer stdout --relay-forward-mailer stdout \
  > "$WORK/auth.log" 2>&1 &
EXTRA_PIDS+=($!)
for _ in $(seq 1 40); do
  [ "$(code "$AUTH_URL/readyz")" = "200" ] && break; sleep 1
done

echo ""
echo "=== The pair, on every platform service ==="
assert_healthy_pair "control " "http://localhost:$ZEROSHIP_CONTROL_PORT"
assert_healthy_pair "gateway " "http://localhost:$ZEROSHIP_GATEWAY_PORT"
assert_healthy_pair "worker  " "http://localhost:$ZEROSHIP_WORKER_PORT"
assert_healthy_pair "migrated" "http://localhost:$MIGRATED_PORT"
assert_healthy_pair "auth    " "$AUTH_URL"

echo ""
echo "=== Down case A: gateway that has never reached a control plane ==="
# The gateway's readiness is a different MECHANISM from the Postgres services':
# it reads the freshness stamp of the background route pull instead of probing
# on demand. A suite that only took Postgres away would leave that path
# unmeasured. ONE variable differs from the healthy gateway above: --control-url
# points at a port nothing listens on.
"$BIN/zeroship-gate" --port "$GATEWAY_DOWN_PORT" \
  --control-url "http://127.0.0.1:$DEAD_PORT" \
  --worker-urls "http://localhost:$ZEROSHIP_WORKER_PORT" --blob-store "$WORK/blobs" \
  --blob-cache-disk-root "$WORK/blob-cache-down" --poll-interval 2 \
  --signing-key-file "$WORK/signing-key.pem" \
  --broker-secret-file "$WORK/gate-secret" \
  > "$WORK/gate-down.log" 2>&1 &
EXTRA_PIDS+=($!)
GDOWN="http://localhost:$GATEWAY_DOWN_PORT"
for _ in $(seq 1 30); do [ "$(code "$GDOWN/healthz")" = "200" ] && break; sleep 1; done
# Several failed poll cycles at --poll-interval 2.
sleep 8

C="$(code "$GDOWN/healthz")"
[ "$C" = "200" ] && pass "gateway (no control) /healthz still 200 - liveness ignores deps" \
  || { fail "gateway (no control) /healthz expected 200, got $C"; tail -20 "$WORK/gate-down.log"; }
C="$(code "$GDOWN/readyz")"
[ "$C" = "503" ] && pass "gateway (no control) /readyz 503 - never pulled a route table" \
  || fail "gateway (no control) /readyz expected 503, got $C"

echo ""
echo "=== Down case B: the real thing - stop Postgres under the live stack ==="
# Everything above proved the healthy arm, which on its own is indistinguishable
# from a /readyz that returns 200 unconditionally. This is the arm that tells
# them apart, and it is a REAL outage rather than a mis-pointed flag: the same
# processes that just answered 200 now answer 503, with only the database
# having changed. /healthz on each is the paired control.
docker stop "$PG_CONTAINER" >/dev/null 2>&1 \
  && pass "ephemeral Postgres stopped" || fail "could not stop $PG_CONTAINER"
# Longer than the readiness cache TTL, so the next probe re-runs rather than
# serving the 200 it cached a moment ago.
sleep 4

for svc in "control :$ZEROSHIP_CONTROL_PORT" "migrated:$MIGRATED_PORT" "auth    :$AUTH_PORT"; do
  name="${svc%%:*}"; port="${svc##*:}"
  C="$(code "http://localhost:$port/healthz")"
  [ "$C" = "200" ] && pass "$name /healthz still 200 with Postgres gone" \
    || fail "$name /healthz expected 200 with Postgres gone, got $C"
  C="$(code "http://localhost:$port/readyz")"
  [ "$C" = "503" ] && pass "$name /readyz 503 with Postgres gone" \
    || fail "$name /readyz expected 503 with Postgres gone, got $C"
done

echo ""
echo "=== Down case C: the background-polled services age out ==="
# Worker and gateway do not touch Postgres, so they stay ready until their
# upstream poll goes stale: control now 500s /internal/{versions,routes}
# because IT cannot reach the database. This is the "was fresh, then aged out"
# arm, which down case A (never fresh) cannot reach.
for name_port in "worker :$ZEROSHIP_WORKER_PORT" "gateway:$ZEROSHIP_GATEWAY_PORT"; do
  name="${name_port%%:*}"; port="${name_port##*:}"
  aged=0
  for _ in $(seq 1 40); do
    [ "$(code "http://localhost:$port/readyz")" = "503" ] && { aged=1; break; }
    sleep 1
  done
  [ "$aged" = "1" ] && pass "$name /readyz aged out to 503 after control lost its database" \
    || fail "$name /readyz never went 503 within 40s of the control plane failing"
  C="$(code "http://localhost:$port/healthz")"
  [ "$C" = "200" ] && pass "$name /healthz still 200 throughout" \
    || fail "$name /healthz expected 200, got $C"
done

echo ""
echo "=== Summary ==="
echo "  pass=$PASS fail=$FAIL"
[ "$FAIL" -eq 0 ] || exit 1
exit 0
