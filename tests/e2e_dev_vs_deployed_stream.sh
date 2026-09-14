#!/usr/bin/env bash
# Verify native RPC streams preserve incremental delivery, framing, and
# response headers through Vite development and the deployed gateway path.
# MUTATE_BUFFERED removes curl's unbuffered mode to prove the timing verdict
# rejects an aggregated response.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release"
# shellcheck source=tests/lib/runtime_secrets.sh
source "$ROOT/tests/lib/runtime_secrets.sh"
APP_DIR="$ROOT/examples/stream-probe"
ZSHIP="$APP_DIR/dist/app.zship"
WORK="$(mktemp -d)"

PG_CONTAINER="${PG_CONTAINER:-compose-postgres-1}"
PG_USER="${PG_USER:-postgres}"
# Per-RUN database name, so a second run of this harness cannot DROP ... WITH
# (FORCE) this one's database out from under it fifteen minutes in. FORCE
# terminates every other backend on the database first, so against a fixed name
# the drop always succeeds - including when the other backend is that second
# run. tests/lib/scratch_db.sh carries the measured collision and
# `tests/lib_scratch_db_selftest.sh` covers both directions.
#
# PG_DB stays the caller's knob and a database the caller named is NEVER
# dropped on exit - that is how you inspect a failed run. DATABASE_URL is
# deliberately NOT read for the name: it has never been coupled to PG_DB here
# (the recreate targets $PG_DB while the services get $DATABASE_URL), and
# deriving the name from it would aim WITH (FORCE) at whatever that DSN names.
# shellcheck source=tests/lib/scratch_db.sh
. "$ROOT/tests/lib/scratch_db.sh"
TEST_DB="${TEST_DB:-${PG_DB:-}}"
zs_scratch_db_resolve zeroship_stream || exit $?
PG_DB="$TEST_DB"
DB_URL="${DATABASE_URL:-postgres://postgres:zeroship@localhost:5440/$PG_DB}"
# zs_scratch_db_cleanup reaches the server through `run_psql`; without it the
# generated database is leaked and the library says so rather than pretending.
run_psql() { docker exec "$PG_CONTAINER" psql -U "$PG_USER" "$@"; }
ZEROSHIP_CONTROL_PORT="${ZEROSHIP_CONTROL_PORT:-9394}"
ZEROSHIP_WORKER_PORT="${ZEROSHIP_WORKER_PORT:-8394}"
ZEROSHIP_GATEWAY_PORT="${ZEROSHIP_GATEWAY_PORT:-8304}"
# This harness starts control and worker directly instead of using
# stack_workspace. Declare the loopback worker-enrolment envelope here so the
# worker can obtain its instance identity before the gateway sends it traffic.
ZEROSHIP_CONTROL_WORKER_ENROLMENT_NETWORKS="${ZEROSHIP_CONTROL_WORKER_ENROLMENT_NETWORKS:-127.0.0.0/8}"
ZEROSHIP_CONTROL_WORKER_ENROLMENT_PORTS="${ZEROSHIP_CONTROL_WORKER_ENROLMENT_PORTS:-$ZEROSHIP_WORKER_PORT}"
export ZEROSHIP_CONTROL_WORKER_ENROLMENT_NETWORKS ZEROSHIP_CONTROL_WORKER_ENROLMENT_PORTS
DEV_PORT="${DEV_PORT:-3061}"
# VITE's own port. DEV_PORT above is the RUNTIME port -- what `zeroship serve`
# binds, and the only one the app's vite.config names. Left undeclared until
# 2026-08-11, vite silently took its :5173 GLOBAL default: nothing here knew
# that port, so cleanup could not free it and vite outlived every run orphaned
# at PPID 1. Measured on the workflows harness before its identical fix
# (590aeab85) -- one such process, 38 minutes old, still holding 127.0.0.1:5173.
# :5173 is also every other vite's default, so two harnesses at once fought over
# it (#173's class). Private, and --strictPort at the call so a conflict fails
# loudly rather than moving to a port nobody watches. See #272.
#
# 5062, NOT 5061. The other harnesses mirror DEV_PORT 30NN -> 50NN, and 3061
# would give 5061 -- which is a BLOCKED PORT. The runtime's own fetch enforces
# the WHATWG bad-ports list (crates/zeroship-runtime/src/web/fetch/bad_ports.rs:30 lists
# 5060 and 5061, sip/sips), and the dev runtime fetches modules FROM vite, so
# the app never loads. Measured, not guessed: with 5061 this harness failed
#     FAIL dev app never came up
# and the runtime logged, twenty times,
#     Network request failed: network error: blocked port 5061
# Checked the rest of the mapping against that list too -- 5011, 5021, 5081,
# 5091, 5092, 5093, 5097 are all clear; 5061 was the only collision.
VITE_PORT="${VITE_PORT:-5062}"
export E2E_STALE_WORKER_BEARER="${E2E_STALE_WORKER_BEARER:-stream-worker-key-0123456789abcdefgh}"
APP_NAME="streamp"
# Set to 1 to run the buffering mutation described in the header.
MUTATE_BUFFERED="${MUTATE_BUFFERED:-0}"

PASS=0; FAIL=0; PIDS=()
ok() { PASS=$((PASS+1)); echo "  ok   $1"; }
no() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }
cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  # BOTH ports, by LISTENER not by recorded PID: `( cd x && vite )&` records the
  # subshell, and vite outlives it as an orphan. Freeing DEV_PORT alone kills
  # `zeroship serve` and leaves vite holding its own port forever.
  for _p in "$DEV_PORT" "$VITE_PORT"; do
    lsof -ti :"$_p" 2>/dev/null | xargs -r kill 2>/dev/null || true
  done
  # Drops the per-run database. A no-op when the caller named it.
  zs_scratch_db_cleanup
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
  "crates/zeroship-runtime/src crates/zeroship-worker/src crates/zeroship-gateway/src crates/zeroship-control/src sdks/rpc/src" \
  "zeroship zeroship-worker zeroship-gate zeroship-control dev-provision" \
  || { _zs_fresh_rc=$?; [ "$_zs_fresh_rc" -ne 0 ] && exit "$_zs_fresh_rc"; }

# Drive the paced stream and print "<ms> <line>" per chunk. -N is what makes
# curl emit each chunk as it arrives; without it curl aggregates and every
# line appears at the end, which is the mutation.
drive() {
  local url="$1" out="$2"
  local nflag=(-N)
  [ "$MUTATE_BUFFERED" = "1" ] && nflag=()
  local start; start=$(date +%s%N)
  # `-D` captures the RESPONSE HEADERS to a sidecar file. They are the other
  # half of the buffering question: a clock says when bytes arrived, a
  # `content-length` says the whole body was known before the first one was.
  curl -sS "${nflag[@]}" -D "$out.hdr" -m 25 -X POST -H 'content-type: application/json' \
    -H 'accept: text/event-stream' "$url" -d '{"json":{}}' 2>&1 \
  | while IFS= read -r line; do
      local now; now=$(date +%s%N)
      printf '%s %s\n' $(( (now - start) / 1000000 )) "$line"
    done > "$out"
}

# ---------------------------------------------------------------------------
# The WIRE FORMAT verdicts. Absolute, per side, and independent of the clock.
# `$f` lines are "<ms> <frame>", so the frame is everything after the first
# space. See the header for why each of these can break while the timing check
# stays green.
# ---------------------------------------------------------------------------
frames() {
  local label="$1" f="$2"
  local frames_file="$f.frames"
  cut -d' ' -f2- < "$f" | grep -v '^$' > "$frames_file"

  # Every tick must ride in a `2:[...]` data frame. Grepping the payload alone
  # would match it wherever it sat.
  local ticks framed
  ticks=$(grep -c '"mark":"TICK"' "$frames_file" || true)
  framed=$(grep -c '^2:\[.*"mark":"TICK"' "$frames_file" || true)
  [ "$ticks" -gt 0 ] && [ "$framed" -eq "$ticks" ] \
    && ok "$label: all $ticks ticks arrived as \`2:[...]\` data frames" \
    || no "$label: $framed of $ticks ticks are in a 2:[...] frame -- frame prefix changed?"

  # The terminator. Exactly one, and LAST: a `d:{}` in the middle would end the
  # stream early for a real client.
  local dcount last
  dcount=$(grep -c '^d:' "$frames_file" || true)
  last=$(tail -1 "$frames_file")
  if [ "$dcount" -eq 1 ] && [ "${last#d:}" != "$last" ]; then
    ok "$label: the stream ENDS with exactly one \`d:\` terminator"
  else
    no "$label: terminator wrong -- $dcount \`d:\` frames, last frame is '${last:-<none>}' (a client waits forever without it)"
  fi

  # An `e:` frame is the dispatcher reporting a throw mid-stream. The ticks
  # would still be there; the run would still look complete.
  grep -q '^e:' "$frames_file" \
    && no "$label: the stream carried an ERROR frame -- $(grep -m1 '^e:' "$frames_file" | cut -c1-200)" \
    || ok "$label: no error frame in the stream"
}

headers() {
  local label="$1" h="$2"
  [ -s "$h" ] || { no "$label: no response headers captured"; return; }
  grep -qi '^content-type: *text/event-stream' "$h" \
    && ok "$label: response declares text/event-stream" \
    || no "$label: content-type is not text/event-stream -- $(grep -i '^content-type' "$h" | tr -d '\r')"
  # A content-length means the body was complete before it was sent. That is
  # buffering, stated by the server, independent of any timing margin.
  grep -qi '^content-length:' "$h" \
    && no "$label: streaming response carries a content-length ($(grep -i '^content-length' "$h" | tr -d '\r')) -- the body was buffered whole" \
    || ok "$label: no content-length on the streaming response"
  # Emitted by the app's fetch handler. If dev has it and deployed does not,
  # the gateway stripped it -- and the next proxy in front of it will buffer.
  grep -qi '^x-accel-buffering: *no' "$h" \
    && ok "$label: x-accel-buffering: no survives to the client" \
    || no "$label: x-accel-buffering: no is MISSING -- an intermediary is free to buffer"
}

# ttfc < total/2 means chunks arrived spread out; ttfc == total means they all
# landed together, which is what buffering looks like from the client.
judge() {
  local label="$1" f="$2"
  local ticks ttfc total
  ticks=$(grep -c '"mark":"TICK"' "$f" || true)
  [ "$ticks" -eq 5 ] && ok "$label: 5 chunks arrived" || { no "$label: got $ticks chunks, want 5"; return; }
  grep -q '"i":1' "$f" && grep -q '"i":5' "$f" && ok "$label: first and last chunk present" \
    || no "$label: chunk numbering incomplete"
  ttfc=$(grep '"mark":"TICK"' "$f" | head -1 | cut -d' ' -f1)
  total=$(grep '"mark":"TICK"' "$f" | tail -1 | cut -d' ' -f1)
  echo "      ttfc=${ttfc}ms total=${total}ms"
  if [ "$total" -gt 0 ] && [ "$((ttfc * 2))" -lt "$total" ]; then
    ok "$label: chunks arrived INCREMENTALLY (ttfc well under half of total)"
  else
    no "$label: chunks arrived TOGETHER (ttfc=${ttfc}ms vs total=${total}ms) -- buffered, not streamed"
  fi
}

echo "=== streaming: dev vs deployed ==="
echo "  MUTATE_BUFFERED=$MUTATE_BUFFERED"
[ "$MUTATE_BUFFERED" = "1" ] && echo "  (MUTATION ACTIVE: curl -N dropped, expect both sides to fail)"

( cd "$APP_DIR" && pnpm build ) > "$WORK/build.log" 2>&1
[ -f "$ZSHIP" ] && ok "built stream-probe .zship" || { no "build produced no .zship"; tail -20 "$WORK/build.log"; exit 1; }
# A missing "use server" builds clean and yields zero procedures (see #167), so
# check the bundle actually carries them rather than trusting the exit code.
grep -q "2 server functions" "$WORK/build.log" && ok "server bundle carries both procedures" \
  || no "server bundle did not report 2 server functions (missing \"use server\"?)"

echo "=== dev side"
for _p in "$DEV_PORT" "$VITE_PORT"; do
  lsof -ti :"$_p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
done
( cd "$APP_DIR" && ./node_modules/.bin/vite --port "$VITE_PORT" --strictPort > "$WORK/dev.log" 2>&1 ) & PIDS+=($!)
# Readiness: a deadline plus a log-derived diagnosis, not a fixed 20 x 2s count
# sized on an idle machine (#273). Sourced HERE and not at the top: e2e_stack.sh
# opens with `: "${ZEROSHIP_CONTROL_PORT:=9120}"` and four more of that shape, which only
# assign when unset, so sourcing it above this harness's own port block would
# hand it the library's ports.
# shellcheck source=/dev/null
source "$ROOT/tests/lib/e2e_stack.sh"
_dev_ping() {
  curl -sf -o /dev/null -m 5 -X POST -H 'content-type: application/json' \
    "http://localhost:$DEV_PORT/__zeroship/v1/probe.ping" -d '{"json":{}}'
}
if stack_wait_dev "dev app" "$WORK/dev.log" _dev_ping; then
  ok "dev app reachable"
else
  no "dev app never became ready -- see the diagnosis and log tail above"
  exit 1
fi
drive "http://localhost:$DEV_PORT/__zeroship/v1/probe.ticks" "$WORK/dev.txt"
judge "dev" "$WORK/dev.txt"
frames "dev" "$WORK/dev.txt"
headers "dev" "$WORK/dev.txt.hdr"

echo "=== deployed side"
for p in $ZEROSHIP_CONTROL_PORT $ZEROSHIP_WORKER_PORT $ZEROSHIP_GATEWAY_PORT; do lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "DROP DATABASE IF EXISTS $PG_DB WITH (FORCE)" >/dev/null 2>&1 || true
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "CREATE DATABASE $PG_DB" >/dev/null 2>&1 || true
zs_platform_migrate "$DB_URL" --migrations-dir "$ROOT/db/migrations-ts" \
  --project-schema zeroship --project-id zeroship > "$WORK/migrate.log" 2>&1 \
  || { no "platform migrations failed"; tail -20 "$WORK/migrate.log"; exit 1; }
ZEROSHIP_GATEWAY_BROKER_SECRET_FILE="$WORK/gate-secret"
e2e_export_runtime_secrets "$WORK" || exit 1
e2e_export_database_urls "$DB_URL"
e2e_start_cdc_relay "$BIN/zeroship-data-cdc-server" || exit 1
"$BIN/zeroship-control" --port "$ZEROSHIP_CONTROL_PORT" --blob-store "$WORK/bundles" \
  > "$WORK/control.log" 2>&1 & PIDS+=($!)
sleep 4
"$BIN/zeroship-worker" --port "$ZEROSHIP_WORKER_PORT" --threads 2 --control-url "http://localhost:$ZEROSHIP_CONTROL_PORT" \
  --blob-store "$WORK/bundles" --poll-interval 2 > "$WORK/worker.log" 2>&1 & PIDS+=($!)
sleep 3
openssl rand -base64 48 > "$WORK/gate-secret"; chmod 600 "$WORK/gate-secret"
"$BIN/zeroship-gate" --port "$ZEROSHIP_GATEWAY_PORT" --control-url "http://localhost:$ZEROSHIP_CONTROL_PORT" \
  --worker-urls "http://localhost:$ZEROSHIP_WORKER_PORT" --blob-store "$WORK/bundles" \
  --broker-secret-file "$WORK/gate-secret" --poll-interval 2 > "$WORK/gate.log" 2>&1 & PIDS+=($!)
sleep 4
curl -sf "http://localhost:$ZEROSHIP_GATEWAY_PORT/readyz" >/dev/null && ok "stack healthy" \
  || { no "stack did not come up"; tail -20 "$WORK/gate.log"; exit 1; }

OUT=$("$BIN/dev-provision" --db "$DB_URL" --blob-store "$WORK/bundles" --name "$APP_NAME" --zship "$ZSHIP" 2>&1)
APP_ID=$(echo "$OUT" | awk -F= '$1=="app_id"{print $2}')
[ -n "$APP_ID" ] && ok "deployed stream-probe" || { no "provision: $OUT"; exit 1; }
sleep 6
curl -sf -o /dev/null -m 10 -X POST -H 'content-type: application/json' \
  "http://localhost:$ZEROSHIP_GATEWAY_PORT/apps/$APP_NAME/__zeroship/v1/probe.ping" -d '{"json":{}}' \
  && ok "deployed app reachable" || no "deployed app did not answer ping"
drive "http://localhost:$ZEROSHIP_GATEWAY_PORT/apps/$APP_NAME/__zeroship/v1/probe.ticks" "$WORK/deployed.txt"
judge "deployed" "$WORK/deployed.txt"
frames "deployed" "$WORK/deployed.txt"
headers "deployed" "$WORK/deployed.txt.hdr"

# The frames and headers verbatim, so a reader can check the verdicts above
# rather than take them.
echo ""
echo "  --- deployed frames (verbatim, <ms> <frame>) ---"
sed 's/^/  /' "$WORK/deployed.txt"
echo "  --- deployed response headers ---"
tr -d '\r' < "$WORK/deployed.txt.hdr" | sed 's/^/  /'

# Keep a floor beside the gate so a path that silently skips its assertions
# cannot pass through an empty result.
STREAM_MIN_PASSED=24

echo ""
echo "  streaming: $PASS passed, $FAIL failed  (MUTATE_BUFFERED=$MUTATE_BUFFERED)  (floor $STREAM_MIN_PASSED)"
echo "  MUTATION: re-run with MUTATE_BUFFERED=1 to drop curl -N; both sides must then FAIL"

# A buffered mutation still runs the assertions it makes fail, so the floor
# counts both outcomes. FAIL remains the behavioral verdict.
RAN=$((PASS + FAIL))
rc=0
[ "$FAIL" -eq 0 ] || rc=1
if [ "$RAN" -lt "$STREAM_MIN_PASSED" ]; then
  echo "FAIL: only $RAN assertions RAN, fewer than the $STREAM_MIN_PASSED this gate expects." >&2
  echo "      (passed $PASS, failed $FAIL -- the floor counts both, because a failing" >&2
  echo "      assertion still ran and is already caught above.)" >&2
  echo "      Assertions do not vanish by accident: either a tier stopped being probed" >&2
  echo "      (judge/frames/headers are each called once per side) or one was removed." >&2
  echo "      If the removal was deliberate, lower STREAM_MIN_PASSED in the same change" >&2
  echo "      and say why; do not treat the gap as slack." >&2
  rc=1
fi
exit "$rc"
