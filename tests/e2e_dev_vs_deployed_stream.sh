#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Streaming, dev vs deployed: do SSE chunk BOUNDARIES survive the gateway?
#
# The failure this guards is a proxy that buffers the response body. Such a
# proxy still delivers every chunk, in order, with identical bytes -- so a test
# that compared only the assembled result would pass while streaming was
# completely broken. Nothing else in this repo would notice.
#
# So the fixture (examples/stream-probe) paces its emission: 5 chunks 200ms
# apart. That makes TIME-TO-FIRST-CHUNK the discriminator. Streaming delivers
# chunk 1 at ~200ms and chunk 5 at ~1000ms; buffering delivers all five at
# ~1000ms. The assertion is ttfc < total/2, which the streaming case clears by
# 2.5x and the buffering case fails outright -- not a tight timing margin.
#
# Proven load-bearing by dropping curl's -N (unbuffered) flag, which makes the
# client aggregate exactly as a buffering proxy would: both sides then report
# ttfc == total and the run fails. See the MUTATION note at the bottom.
#
# WHAT THE TIMING CHECK CANNOT SEE (added 2026-08-09). Unlike its siblings this
# script never diffs the two tiers -- `judge` is already an absolute verdict,
# applied to each side on its own, so a defect shared by both makes BOTH fail.
# But it only ever looked at the TICK lines and the clock. The wire format
# around them was unmeasured, and every part of it can break while five ticks
# still arrive 200ms apart:
#
#   - the TERMINATOR. The stream is AI-SDK data-stream framing
#     (sdks/bootstrap/src/fetch-handler.ts): `2:[<json>]` per chunk and a final
#     `d:{}`. `d:` is what tells the client the stream ENDED
#     (sdks/rpc/src/transport.ts consumeLine), so an omitted terminator leaves
#     a correct-looking stream that a real client waits on until the socket
#     closes. Every tick assertion here stays green through that.
#   - the FRAME PREFIX. `grep '"mark":"TICK"'` matches the payload wherever it
#     sits, so a change from `2:[...]` to anything else is invisible.
#   - the RESPONSE HEADERS. A `content-length` on a streaming response is
#     positive proof of buffering -- stronger evidence than any clock -- and
#     `x-accel-buffering: no` is what stops an intermediary from doing it. The
#     gateway could strip either and the timing check would still pass in this
#     single-hop test rig.
#
# Those are asserted below, on the RAW frames and the RAW headers, per side.
#
# MUTATIONS for that half edit `sdks/bootstrap/src/fetch-handler.ts`, rebuild
# the package and the app, and must move only the verdict named:
#   MUTATE=no-terminator   the `d:{}` frame is never enqueued
#   MUTATE=no-sse-ctype    the response declares application/json
# Both restore the source and rebuild on exit.
#
# THEY REACH THE DEPLOYED TIER ONLY, and that is a finding rather than a
# limitation. MEASURED 2026-08-09: with `no-terminator` applied and
# @zeroship/bootstrap rebuilt, the DEPLOYED stream lost its `d:` frame while
# the DEV stream still had one. The deployed side runs the fetch handler Vite
# bundled into the `.zship` (the server blob carries both the `d:{}` emitter
# and the `x-accel-buffering` header -- checked in the built artifact). The dev
# side does NOT: `sdks/vite-plugin/src/dev-server.ts` boots
# `sdks/vite-plugin/dist/dev-bootstrap.js`, a PREBUILT bundle that inlines its
# own copy of the same handler and is only refreshed when the vite plugin is
# rebuilt.
#
# So the two tiers can be running different VINTAGES of the same framework
# code, and nothing here or in tests/lib/binary_freshness.sh (which watches
# Rust binaries) would say so. For these mutations that is convenient -- the
# dev row becomes a one-variable control -- but for a real change to the
# framework it means a stale `dev-bootstrap.js` shows up as a
# "dev-vs-deployed divergence" that is neither backend's fault.
#
# Prereqs (docs/runbooks/local-dev.md):
#   cargo build --release -p zeroship-control -p zeroship-worker -p zeroship-gateway -p zeroship-cli --bins
#   pnpm install && pnpm build
#   docker (Postgres on :5440 as compose-postgres-1)
#   pnpm install in examples/stream-probe
# ---------------------------------------------------------------------------
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
export ZEROSHIP_WORKER_KEY="${ZEROSHIP_WORKER_KEY:-stream-worker-key-0123456789abcdefgh}"
APP_NAME="streamp"
# Set to 1 to run the buffering mutation described in the header.
MUTATE_BUFFERED="${MUTATE_BUFFERED:-0}"
MUTATE="${MUTATE:-none}"
BOOTSTRAP_SRC="$ROOT/sdks/bootstrap/src/fetch-handler.ts"

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
  # A mutation edited a TRACKED SDK source and rebuilt its dist. Put both back
  # before anything else can read them -- a half-restored bootstrap would make
  # every later run in this tree report on the mutation instead of the product.
  if [ -f "${MUTATE_BAK:-}" ]; then
    cp "$MUTATE_BAK" "$BOOTSTRAP_SRC"
    ( cd "$ROOT" && pnpm --filter @zeroship/bootstrap build ) >/dev/null 2>&1 \
      || echo "  WARNING: bootstrap restore build FAILED -- run 'pnpm --filter @zeroship/bootstrap build' by hand"
  fi
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
  local url="$1" key="${2:-}" out="$3"
  local hdr=() nflag=(-N)
  [ -n "$key" ] && hdr=(-H "X-Api-Key: $key")
  [ "$MUTATE_BUFFERED" = "1" ] && nflag=()
  local start; start=$(date +%s%N)
  # `-D` captures the RESPONSE HEADERS to a sidecar file. They are the other
  # half of the buffering question: a clock says when bytes arrived, a
  # `content-length` says the whole body was known before the first one was.
  curl -sS "${nflag[@]}" -D "$out.hdr" -m 25 -X POST -H 'content-type: application/json' \
    -H 'accept: text/event-stream' "${hdr[@]}" "$url" -d '{"json":{}}' 2>&1 \
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
echo "  mutation: $MUTATE   MUTATE_BUFFERED=$MUTATE_BUFFERED"
[ "$MUTATE_BUFFERED" = "1" ] && echo "  (MUTATION ACTIVE: curl -N dropped, expect both sides to fail)"

# Wire-format mutations. They edit the fetch handler BOTH tiers run, so the
# defect is genuinely present on the deployed side rather than simulated at the
# client, and the rebuild order matters: the package first, then the app that
# bundles it.
if [ "$MUTATE" != "none" ]; then
  MUTATE_BAK="$WORK/fetch-handler.ts.bak"
  cp "$BOOTSTRAP_SRC" "$MUTATE_BAK"
  case "$MUTATE" in
    no-terminator)
      # The terminator frame becomes an empty chunk: the stream still ends and
      # every tick still arrives, but nothing tells the client it is over.
      sed -i 's|encoder.encode("d:{}\\n")|encoder.encode("")|g' "$BOOTSTRAP_SRC"
      grep -q 'encoder.encode("")' "$BOOTSTRAP_SRC" \
        || { no "mutation did not apply to $BOOTSTRAP_SRC"; exit 1; } ;;
    no-sse-ctype)
      sed -i 's|"content-type": "text/event-stream",|"content-type": "application/json",|' \
        "$BOOTSTRAP_SRC"
      grep -q '"content-type": "application/json",$' "$BOOTSTRAP_SRC" \
        || { no "mutation did not apply to $BOOTSTRAP_SRC"; exit 1; } ;;
    *) no "unknown MUTATE=$MUTATE"; exit 2 ;;
  esac
  ( cd "$ROOT" && pnpm --filter @zeroship/bootstrap build ) > "$WORK/bootstrap-build.log" 2>&1 \
    || { no "bootstrap rebuild failed"; tail -20 "$WORK/bootstrap-build.log"; exit 1; }
  echo "  MUTATED ($MUTATE): @zeroship/bootstrap rebuilt -- the DEPLOYED bundle picks"
  echo "    it up; dev keeps the vite plugin's prebuilt dev-bootstrap.js (see header)"
fi

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
drive "http://localhost:$DEV_PORT/__zeroship/v1/probe.ticks" "" "$WORK/dev.txt"
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
KEY=$(echo "$OUT" | awk -F= '$1=="api_key"{print $2}')
[ -n "$KEY" ] && ok "deployed stream-probe" || { no "provision: $OUT"; exit 1; }
sleep 6
curl -sf -o /dev/null -m 10 -X POST -H 'content-type: application/json' -H "X-Api-Key: $KEY" \
  "http://localhost:$ZEROSHIP_GATEWAY_PORT/apps/$APP_NAME/__zeroship/v1/probe.ping" -d '{"json":{}}' \
  && ok "deployed app reachable" || no "deployed app did not answer ping"
drive "http://localhost:$ZEROSHIP_GATEWAY_PORT/apps/$APP_NAME/__zeroship/v1/probe.ticks" "$KEY" "$WORK/deployed.txt"
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

# --- The floor: a MEASURED minimum, and the guard against a green run over ---
#     nothing. This script exits on $FAIL alone, and $FAIL is 0 both when every
#     assertion passed and when NO assertion ran -- a stack that never came up,
#     a `drive` that wrote an empty file, a rename that stopped `judge` being
#     called. This repo has shipped three gates that passed over zero tests
#     (#102/#103/#112); a job that cannot fail is worse than no job.
#
# THE FLOOR IS A MEASUREMENT. Taken 2026-08-10 on this tree, running this script
# unmodified against the compose Postgres on :5440:
#
#     streaming: 24 passed, 0 failed        (exit 0)
#
# CROSS-CHECKED against a second, independent instrument, because a count read
# out of the run it is meant to guard proves only that the run was self-
# consistent. Counting `ok "` CALL SITES in this file and multiplying by the
# number of times each function is invoked: judge() 3, frames() 3, headers() 3,
# each called once per tier = (3+3+3) x 2 = 18, plus 6 top-level setup sites
# (built .zship, 2 server functions, dev reachable, stack healthy, deployed
# stream-probe, deployed reachable) = 24. The dynamic 24 and the static 24
# agree, and they disagree for different reasons if either is wrong: the
# dynamic one moves when a tier stops answering, the static one when an
# assertion is deleted from the source.
#
# NO HEADROOM, deliberately. This total is not DISCOVERED (unlike a cargo run,
# where feature resolution and host capabilities move the count); it is the
# number of ok()/no() sites this file reaches, fixed by the source. Adding an
# assertion passes untouched (25 >= 24); removing one costs a deliberate edit
# here. That asymmetry is the whole point.
#
# WHAT THE FLOOR DOES NOT CATCH: substitution. Deleting one assertion and adding
# an easier one keeps the total at 24. Nothing here can see that; review can.
STREAM_MIN_PASSED=24

echo ""
# MUTATE_BUFFERED is named in this line too. It is a SEPARATE variable from
# MUTATE, so a buffered run used to close with `(mutation: none)` beside two
# failures -- the one line a reader skips to, telling them an unmutated run had
# regressed. The banner above says it, but the banner is 50 lines up.
echo "  streaming: $PASS passed, $FAIL failed  (mutation: $MUTATE, MUTATE_BUFFERED=$MUTATE_BUFFERED)  (floor $STREAM_MIN_PASSED)"
echo "  MUTATION: re-run with MUTATE_BUFFERED=1 to drop curl -N; both sides must then FAIL"
echo "  MUTATIONS (wire format): MUTATE=no-terminator | no-sse-ctype -- each edits"
echo "    sdks/bootstrap/src/fetch-handler.ts; the DEPLOYED side carries the defect"
echo "    and exactly the named verdict must go RED while the tick/timing rows stay"
echo "    green (dev keeps its prebuilt dev-bootstrap.js -- see the header note)"

# A MUTATION run is EXPECTED to fail assertions, so the floor is the only thing
# that still has to hold there: the point of MUTATE_BUFFERED=1 is that the
# timing rows go red, not that fewer rows run. The floor must therefore count
# assertions that RAN. A failing assertion is already caught by `FAIL -eq 0`
# below; the floor exists for the other failure, where an assertion stops
# running at all and its absence reads as a pass. A mutation moves an outcome
# between columns and leaves the sum alone.
#
# MEASURED 2026-08-11, both runs mine, same HEAD:
#   unmutated          24 passed, 0 failed  -> 24 ran
#   MUTATE_BUFFERED=1  22 passed, 2 failed  -> 24 ran
# The two are the incremental-arrival rows, one per tier, exactly the pair the
# mutation names. Counting PASS alone reported `only 22 assertions passed,
# fewer than the 24` on that run -- a third failure that is not one, on the one
# run this harness is designed to be told apart by.
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
