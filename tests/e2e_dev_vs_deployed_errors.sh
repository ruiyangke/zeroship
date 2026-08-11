#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Error envelopes: dev vs deployed -- run ONE identical sequence of throwing RPC
# calls against `pnpm dev` and against the same app deployed behind the gateway,
# then (a) diff the RESULTS and (b) assert an ABSOLUTE property of the DEPLOYED
# bodies.
#
# THE QUESTION THIS ANSWERS: does a zeroship app leak `Error.stack` to its
# clients in PRODUCTION, or only in dev?
#
# WHY (b) EXISTS, AND WHY (a) ALONE CANNOT ANSWER THAT.
#   `tests/e2e_dev_vs_deployed_auth.sh` already drives worker-originated errors
#   (`probe.requireAnon`, `probe.appGate` are anon-reachable and throw). But its
#   `scrub` collapses `"stack":"..."` to `"stack":"<STACK>"` on BOTH sides
#   before diffing -- correctly, because dev frames are this machine's absolute
#   source paths and deployed frames are minified bundle offsets, so the CONTENT
#   can never match and is not a contract. The consequence is that when both
#   tiers emit a stack the rows are byte-identical after scrubbing and the diff
#   is GREEN. A relative comparison is structurally incapable of reporting "both
#   tiers leak". That is why this went unmeasured, and it is why this harness
#   asserts the absolute property separately, against the RAW deployed bytes.
#
#   Symmetrically, a crate-local suite cannot see it either:
#   `crates/plugin-db/tests/capability.rs:41` sets `AUTH_INSECURE_DEV=true` on
#   purpose so it can read the verbose envelope -- it opts OUT of the very rail
#   under examination.
#
# THE FIXTURE (examples/error-probe) HOLDS EVERYTHING CONSTANT BUT ONE THING.
#   All four throwing procedures share one throw site and one message, so rows
#   differ only in which properties ride on the thrown error:
#
#     err.plain          {}                          -> 500, sanitizer's blank arm
#     err.status4xx      {status:403}                -> one variable vs err.plain
#     err.status4xxCode  {status:403,code:...}       -> one variable vs err.status4xx
#     err.publicCode5xx  {code:"version_mismatch"}   -> one variable vs err.plain,
#                                                       and that code is on
#                                                       `is_public_error_code`, so
#                                                       it takes the 5xx
#                                                       sanitizer's EXEMPTION arm
#     err.ok             (throws nothing)            -> the pass-through control
#
#   `err.ok` is load-bearing. Every "the deployed body has no stack" verdict is
#   reported ONLY alongside it, because without it a green is equally consistent
#   with the request having 404'd, been blocked at the gateway, or returned an
#   empty body. err.ok carries the SAME marker string as the error messages, so
#   it proves that exact string survives the whole transport.
#
# CAN THIS COMPARISON FAIL? Two guards.
#   1. A dev-vs-dev self-diff that must be EMPTY (the auth harness's guard: a
#      comparison that is red no matter what proves as little as one that is
#      green no matter what).
#   2. MUTATE=dev-insecure, which sets AUTH_INSECURE_DEV=true for the DEV SIDE
#      ONLY -- one variable, one side -- and must drive the diff RED on exactly
#      the 5xx rows. That is the red-before-green for the relative half.
#   3. MUTATE=drop-status, which removes `status: 403` from the shared source so
#      err.status4xx lands at 500 on BOTH sides. The relative diff stays green;
#      the ABSOLUTE verdict for that row must flip. That is the row-level
#      sensitivity control for the absolute half -- it proves the leak verdict
#      tracks the status class rather than being hardcoded per procedure name.
#
# Prereqs (docs/runbooks/local-dev.md):
#   pnpm build
#   cargo build --release -p zeroship-control -p zeroship-worker -p zeroship-gateway -p zeroship --bins
#   cargo build --release -p zeroship-migrate-adapter --features platform-cli --bin zeroship-platform-migrate
#   docker (this script starts its OWN ephemeral Postgres -- nothing to pre-start)
#   pnpm install --filter ./examples/error-probe...
#
#   ./tests/e2e_dev_vs_deployed_errors.sh
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release"
APP="$ROOT/examples/error-probe"
ZSHIP="$APP/dist/app.zship"

# A port band of its own: auth 9398/8398/8308/5458, kv 9392/8392/8302/3011,
# storage 9396/8396/8306/3081, streaming 9394/8394/8304/3061.
export CONTROL_PORT="${CONTROL_PORT:-9399}"
export WORKER_PORT="${WORKER_PORT:-8399}"
export GATE_PORT="${GATE_PORT:-8309}"
export PG_PORT="${PG_PORT:-5459}"
export PG_CONTAINER="${PG_CONTAINER:-zs-devdeploy-err-pg}"
DEV_PORT="${DEV_PORT:-3093}"   # examples/error-probe ERROR_PROBE_API_PORT default
# VITE's own port. DEV_PORT above is the RUNTIME port. vite was silently taking
# its :5173 global default, which nothing here declared, tracked or freed, so a
# second harness on this machine fought it for the port and the cleanup trap
# could never reclaim it. --strictPort at the call so a conflict fails loudly
# rather than moving to a port nobody watches. Checked against the runtime's
# bad-ports list before choosing it (see #272 and the stream harness). Both vite
# call sites below (mutated and unmutated) must carry it. See #272.
VITE_PORT="${VITE_PORT:-5093}"
APP_SLUG="error-probe-dd"
HOST="$APP_SLUG.localhost"
MUTATE="${MUTATE:-none}"

# Must match examples/error-probe/src/index.ts LEAK_MARKER. Re-asserted against
# that file below so the pair cannot drift silently -- a drifted marker would
# make every leak assertion trivially green.
LEAK_MARKER="ZSLEAK-b7f1-marker"
# The throw-site function name. Present in dev frames (unminified sources);
# absent from deployed frames by construction (the server bundle is minified to
# one line), which is why the absolute assertion keys on the `stack` KEY rather
# than on this identifier.
FRAME_MARKER="zsLeakFrameMarker"

PROCS="err.plain err.status4xx err.status4xxCode err.publicCode5xx err.ok"

PASS=0; FAIL=0; PIDS=()
pass() { PASS=$((PASS+1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }

source "$ROOT/tests/lib/e2e_stack.sh"
cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  # BOTH ports, by LISTENER not by recorded PID: `( cd x && vite )&` records the
  # subshell, and vite outlives it as an orphan the PID loop cannot reach.
  for _p in "$DEV_PORT" "$VITE_PORT"; do
    lsof -ti :"$_p" 2>/dev/null | xargs -r kill 2>/dev/null || true
  done
  [ -f "${MUTATE_BAK:-}" ] && cp "$MUTATE_BAK" "$APP/src/index.ts"
  if [ "${KEEP_WORK:-0}" = "1" ]; then
    echo "  work dirs kept: dev=${DEV_WORK:-${WORK_EARLY:-<none>}} deployed=${WORK:-<none>}"
    if [ -n "${PIDFILE:-}" ] && [ -f "$PIDFILE" ]; then
      while read -r p; do [ -n "$p" ] && kill "$p" 2>/dev/null || true; done < "$PIDFILE"
    fi
    docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
  else
    stack_down 2>/dev/null || true
    rm -rf "${WORK_EARLY:-}"
  fi
}
trap cleanup EXIT

# --- 0. the two sides must be the same BUILD --------------------------------
# shellcheck source=lib/binary_freshness.sh
source "$ROOT/tests/lib/binary_freshness.sh"
zs_check_binary_freshness "$ROOT" "$BIN" \
  "crates/runtime/src crates/worker/src crates/gateway/src crates/control/src crates/core/src sdks/bootstrap/src sdks/rpc/src" \
  "zeroship zeroship-worker zeroship-gate zeroship-control" \
  || { _zs_fresh_rc=$?; [ "$_zs_fresh_rc" -ne 0 ] && exit "$_zs_fresh_rc"; }

echo "=== error envelopes: dev vs deployed (error-probe) ==="
echo "  mutation: $MUTATE"

# --- 0b. THE PRECONDITION THE WHOLE MEASUREMENT RESTS ON --------------------
# `AUTH_INSECURE_DEV` is the documented escape hatch that restores the verbose
# 5xx body (crates/runtime/src/core/dispatch.rs::expose_internal_dispatch_errors,
# sdks/bootstrap/src/fetch-handler.ts::insecureDevErrorsEnabled). If it leaks
# into this shell it reaches BOTH tiers through the environment and every 5xx
# row below measures the escape hatch instead of the production rail -- silently,
# and in the direction that manufactures a finding. Refuse to run rather than
# report on it. (MUTATE=dev-insecure sets it for the dev child ONLY, explicitly,
# after this gate.)
if [ -n "${AUTH_INSECURE_DEV:-}" ]; then
  fail "AUTH_INSECURE_DEV is set in this shell ('${AUTH_INSECURE_DEV}') -- that is the escape hatch under examination; unset it and re-run"
  exit 2
fi
pass "AUTH_INSECURE_DEV unset in the harness environment (production rail is live)"

# ---------------------------------------------------------------------------
# The probe. ONE function, both sides. `$1` is the base URL.
#
# Every line is `<proc> <status> <body>`. TWO outputs are produced from the same
# calls:
#   - stdout           : the CONTRACT row, with the stack collapsed to <STACK>
#                        so it is comparable across tiers. This feeds the diff.
#   - $RAWFILE         : the body VERBATIM. This feeds the absolute assertions
#                        and is what gets quoted in the report.
# ---------------------------------------------------------------------------
probe() {
  local base="$1" proc
  local rpc="$base/__zeroship/v1"

  # Blank only what a clock or a random id makes volatile. `request_id` is in
  # the sanitized 5xx body and is per-request by construction.
  scrub() {
    sed -E \
      -e 's/"stack":"[^"]*"/"stack":"<STACK>"/g' \
      -e 's/"request_id":"[^"]*"/"request_id":"<RID>"/g' \
      -e 's/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/<UUID>/g'
  }

  for proc in $PROCS; do
    local code
    code="$(curl -s -o "$WORK/probe.body" -w '%{http_code}' -m 20 -X POST \
              -H 'content-type: application/json' -H "Host: $HOST" \
              "$rpc/$proc" -d '{"json":{}}' 2>&1)"
    # RAW capture, verbatim, one line per proc.
    printf '%-18s %s %s\n' "$proc" "$code" "$(tr -d '\n' < "$WORK/probe.body")" >> "$RAWFILE"
    # CONTRACT row, scrubbed.
    printf '%-18s %s %s\n' "$proc" "$code" "$(tr -d '\n' < "$WORK/probe.body" | scrub)"
  done
}

# ---------------------------------------------------------------------------
# 1. Build the app, and assert the FIXTURE INVARIANTS the comparison rests on.
# ---------------------------------------------------------------------------
WORK_EARLY="$(mktemp -d -t zs-errprobe-XXXXXX)"
WORK="$WORK_EARLY"   # stack_up replaces this; the build needs a scratch dir now

if [ "$MUTATE" = "drop-status" ]; then
  # ROW-LEVEL SENSITIVITY CONTROL for the ABSOLUTE half. Remove `status: 403`
  # from err.status4xx, changing ONE variable, so it lands at 500 like
  # err.plain. Both sides rebuild from the shared source, so the RELATIVE diff
  # must stay green -- and the leak verdict for that row must FLIP. A verdict
  # that does not move is hardcoded to the procedure name and measures nothing.
  MUTATE_BAK="$WORK/index.ts.bak"
  cp "$APP/src/index.ts" "$MUTATE_BAK"
  sed -i 's|zsLeakFrameMarker({ status: 403 })|zsLeakFrameMarker({})|' "$APP/src/index.ts"
  grep -q 'zsLeakFrameMarker({ status: 403 })' "$APP/src/index.ts" \
    && { fail "mutation did not apply"; exit 1; } \
    || echo "  MUTATED: err.status4xx no longer carries status:403 (both builds)"
fi

( cd "$APP" && pnpm build ) > "$WORK/build.log" 2>&1
[ -f "$ZSHIP" ] && pass "built app.zship ($(du -k "$ZSHIP" | cut -f1)KB)" \
  || { fail "build produced no app.zship"; tail -30 "$WORK/build.log"; exit 1; }

# The marker in this script MUST equal the one the app throws, or every
# assertion keyed on it is vacuously green.
grep -qF "\"$LEAK_MARKER\"" "$APP/src/index.ts" \
  && pass "leak marker '$LEAK_MARKER' matches examples/error-probe/src/index.ts" \
  || { fail "marker drift: '$LEAK_MARKER' is not in $APP/src/index.ts"; exit 1; }

# EVERY procedure must be anon in the BUILT manifest. If any is gated, the
# gateway answers it and the deployed row is the GATEWAY's envelope, not the
# worker's -- which is precisely the blind spot this harness exists to close,
# and it would read as a clean "no stack" rather than as "never measured".
d="$WORK/unpack"; mkdir -p "$d"; tar -xf "$ZSHIP" -C "$d"
anon_ok=1
for p in $PROCS; do
  grep -qE "\"rpc:${p//./\\.}\":\{[^}]*\"auth\":\"anon\"" "$d/manifest.json" \
    || { fail "$p is not anon in the manifest -- deployed calls never reach the worker for it"; anon_ok=0; }
done
[ "$anon_ok" = "1" ] && pass "all $(echo $PROCS | wc -w) procedures are anon in the manifest (deployed calls reach the WORKER)"

# ---------------------------------------------------------------------------
# 2. Dev side. `pnpm dev` spawns `zeroship serve`; the runtime's
#    `build_error_body` lives in that child.
# ---------------------------------------------------------------------------
for _p in "$DEV_PORT" "$VITE_PORT"; do
  lsof -ti :"$_p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
done
DEV_ENV=()
if [ "$MUTATE" = "dev-insecure" ]; then
  # RED-BEFORE-GREEN for the RELATIVE half. One variable, ONE SIDE: the dev
  # child gets the escape hatch, deployed does not. The 5xx rows MUST diverge.
  DEV_ENV=(AUTH_INSECURE_DEV=true)
  echo "  MUTATED: dev child gets AUTH_INSECURE_DEV=true (deployed does NOT)"
fi
# `env "${DEV_ENV[@]:-}"` would expand to `env ''` when the array is empty and
# fail with "env: '': No such file or directory". Branch instead.
if [ "${#DEV_ENV[@]}" -gt 0 ]; then
  ( cd "$APP" && env "${DEV_ENV[@]}" ./node_modules/.bin/vite --port "$VITE_PORT" --strictPort > "$WORK/dev.log" 2>&1 ) & PIDS+=($!)
else
  ( cd "$APP" && ./node_modules/.bin/vite --port "$VITE_PORT" --strictPort > "$WORK/dev.log" 2>&1 ) & PIDS+=($!)
fi
for _ in $(seq 1 25); do
  curl -sf -o /dev/null -m 2 -X POST -H 'content-type: application/json' \
    "http://localhost:$DEV_PORT/__zeroship/v1/err.ok" -d '{"json":{}}' && break
  sleep 2
done
curl -sf -o /dev/null -m 5 -X POST -H 'content-type: application/json' \
  "http://localhost:$DEV_PORT/__zeroship/v1/err.ok" -d '{"json":{}}' \
  && pass "dev app reachable on :$DEV_PORT" \
  || { fail "dev app never came up"; tail -30 "$WORK/dev.log"; exit 1; }

RAWFILE="$WORK/dev.raw"; : > "$RAWFILE"
probe "http://localhost:$DEV_PORT" > "$WORK/dev.txt" 2>&1
grep -q 'err.plain' "$WORK/dev.txt" && pass "dev answered the probe ($(wc -l < "$WORK/dev.txt") rows)" \
  || { fail "dev probe produced nothing"; tail -20 "$WORK/dev.log"; exit 1; }

# --- can this comparison PASS? ---------------------------------------------
# Run the identical probe against the identical server a second time: the two
# outputs MUST be byte-identical. That establishes that the probe is
# deterministic and that `diff` reports nothing when the two inputs agree, so
# every row still differing later is content, not instrument.
RAWFILE="$WORK/dev-again.raw"; : > "$RAWFILE"
probe "http://localhost:$DEV_PORT" > "$WORK/dev-again.txt" 2>&1
if diff -q "$WORK/dev.txt" "$WORK/dev-again.txt" >/dev/null 2>&1; then
  pass "probe is deterministic: dev-vs-dev self-diff is empty (the comparison CAN go green)"
else
  fail "dev disagrees with ITSELF across two runs -- the rows below are instrument noise, not findings"
  diff "$WORK/dev.txt" "$WORK/dev-again.txt" | head -20
fi

# ---------------------------------------------------------------------------
# 3. Deployed side: real stack, real deploy, real gateway.
# ---------------------------------------------------------------------------
DEV_WORK="$WORK"
stack_up || { fail "stack bring-up failed"; exit 1; }   # stack_up resets $WORK
cp "$DEV_WORK/dev.txt" "$WORK/dev.txt"
cp "$DEV_WORK/dev.raw" "$WORK/dev.raw"
mint_admin_pat || exit 1

APP_ID="$(deploy_zship "$APP_SLUG" "$ZSHIP")" || { fail "deploy error-probe"; exit 1; }
pass "deployed error-probe ($APP_ID)"

# The DEPLOYED worker must NOT have the escape hatch in its environment, or the
# "production" half of this comparison is measuring dev behaviour. `--dev-insecure`
# (which stack_up does pass) is a CLI flag on a different subsystem and does not
# set this var -- assert that rather than assume it, by reading the live process.
worker_pid="$(sed -n '2p' "$PIDFILE")"
if [ -n "$worker_pid" ] && [ -r "/proc/$worker_pid/environ" ]; then
  # NOT `tr ... | grep -q`, for a LATENT size-dependence rather than an observed
  # failure. See the long note at the same site in e2e_dev_vs_deployed_env.sh for
  # the measurements; the short form:
  #
  # `grep -q` exits on the FIRST match. If `tr` is still writing then, it takes
  # SIGPIPE (141) and `set -o pipefail` promotes 141 to the pipeline's status --
  # so the `if` takes the else arm EXACTLY WHEN THE VALUE IS PRESENT. But `tr` is
  # only still writing when the data exceeds what the pipe buffer absorbs, so the
  # inversion is CONDITIONAL ON INPUT SIZE. Measured 2026-08-10: it appears
  # between 71 KB and 134 KB, while real `/proc/PID/environ` on the processes
  # this site reads is ~50 KB (50605 / 50917 / 49455 bytes measured). So it does
  # not fire here today; the margin is about 1.5x, and more app vars or a fatter
  # CI image eat it with nothing failing loudly on the way. Removing the pipeline
  # removes the size-dependence at no cost, which is the whole reason to do it.
  #
  # NOT ESTABLISHED, and asserted here before: that this check was ever OBSERVED
  # reporting a worker clean while AUTH_INSECURE_DEV was set. That claim cited a
  # positive control I could not reproduce, and the sizes above do not explain
  # it. If it was real it had another cause, still unexplained -- which matters,
  # because this assertion is the one standing between a dev-only escape hatch
  # and the deployed tier.
  tr '\0' '\n' < "/proc/$worker_pid/environ" > "$WORK/worker.environ" 2>/dev/null || true
  if grep -q '^AUTH_INSECURE_DEV=' "$WORK/worker.environ"; then
    fail "the DEPLOYED worker (pid $worker_pid) has AUTH_INSECURE_DEV set -- it is not running the production rail"
  else
    pass "deployed worker (pid $worker_pid) has NO AUTH_INSECURE_DEV (production rail confirmed on the live process)"
  fi
else
  fail "could not read /proc/$worker_pid/environ -- cannot confirm the deployed worker runs the production rail"
fi

# Wait for the gateway to pull the route before probing (an unrouted call is a
# 404/503 and would read as a divergence when it is a race).
ready=0
for _ in $(seq 1 25); do
  c="$(curl -s -o /dev/null -w '%{http_code}' -m 10 -X POST -H 'content-type: application/json' \
      -H "Host: $HOST" "http://localhost:$GATE_PORT/__zeroship/v1/err.ok" -d '{"json":{}}')"
  [ "$c" = "200" ] && { ready=1; break; }
  sleep 2
done
[ "$ready" = "1" ] && pass "gateway routes to the deployed app (err.ok -> 200)" \
  || { fail "gateway never routed to the app (last code=$c)"; tail -20 "$WORK/gate.log"; }

RAWFILE="$WORK/deployed.raw"; : > "$RAWFILE"
probe "http://localhost:$GATE_PORT" > "$WORK/deployed.txt" 2>&1
grep -q 'err.plain' "$WORK/deployed.txt" && pass "deployed app answered the probe ($(wc -l < "$WORK/deployed.txt") rows)" \
  || { fail "deployed probe produced nothing"; tail -20 "$WORK/worker.log"; }

# ---------------------------------------------------------------------------
# 4a. THE PASS-THROUGH CONTROL. Read this before believing any verdict in 4b.
# ---------------------------------------------------------------------------
echo ""
echo "--- pass-through control (deployed) ---"
ok_row="$(grep '^err\.ok ' "$WORK/deployed.raw")"
echo "  $ok_row"
if printf '%s' "$ok_row" | grep -q "200 .*$LEAK_MARKER"; then
  pass "CONTROL: deployed err.ok returns 200 carrying '$LEAK_MARKER' verbatim"
  CONTROL_OK=1
else
  fail "CONTROL: deployed err.ok did NOT return the marker -- every 'no stack' verdict below is unsafe (the body may simply be empty/blocked)"
  CONTROL_OK=0
fi

# ---------------------------------------------------------------------------
# 4b. THE ABSOLUTE QUESTION: does the DEPLOYED body carry a stack?
#     This is what the relative diff structurally cannot report.
# ---------------------------------------------------------------------------
echo ""
echo "--- does the DEPLOYED error body carry a stack? ---"
leaks=0
for p in err.plain err.status4xx err.status4xxCode err.publicCode5xx; do
  row="$(grep "^$p " "$WORK/deployed.raw")"
  st="$(printf '%s' "$row" | awk '{print $2}')"
  if printf '%s' "$row" | grep -q '"stack":'; then
    leaks=$((leaks+1))
    fail "LEAK: deployed $p (HTTP $st) ships a \"stack\" to the client"
    printf '        %s\n' "$(printf '%s' "$row" | cut -c1-320)"
  else
    pass "deployed $p (HTTP $st) ships NO \"stack\""
  fi
done
if [ "$CONTROL_OK" != "1" ]; then
  echo "  (verdicts above are UNSAFE: the pass-through control failed)"
fi

# Message leakage is a separate question from stack leakage -- an app's own
# message may well be intended for its own client. Reported, not judged.
echo ""
echo "--- does the DEPLOYED error body carry the thrown message? ---"
for p in err.plain err.status4xx err.status4xxCode err.publicCode5xx; do
  row="$(grep "^$p " "$WORK/deployed.raw")"
  if printf '%s' "$row" | grep -qF "$LEAK_MARKER"; then
    echo "  message PRESENT  $p"
  else
    echo "  message ABSENT   $p"
  fi
done

# The dev-side frame identifier, for the record: it is what a stack looks like
# when sources are not minified.
echo ""
if grep -qF "$FRAME_MARKER" "$WORK/dev.raw"; then
  echo "  dev frames name the throw site ($FRAME_MARKER): the dev stack is source-level"
fi

# ---------------------------------------------------------------------------
# 5. THE RELATIVE COMPARISON: identical operations, identical results?
#    NOTE: green here does NOT mean no leak. It means the two tiers AGREE.
#    Section 4b is the one that answers the leak question.
# ---------------------------------------------------------------------------
echo ""
if diff -q "$WORK/dev.txt" "$WORK/deployed.txt" >/dev/null 2>&1; then
  pass "dev and deployed agree on every probed error envelope"
else
  n=$(diff "$WORK/dev.txt" "$WORK/deployed.txt" | grep -c '^<')
  fail "dev and deployed DIVERGE on $n of $(wc -l < "$WORK/dev.txt") rows (< dev, > deployed)"
  diff "$WORK/dev.txt" "$WORK/deployed.txt"
fi

echo ""
echo "  --- raw deployed bodies (verbatim) ---"
sed 's/^/  /' "$WORK/deployed.raw"
echo "  --- raw dev bodies (verbatim, stacks truncated to 200 chars) ---"
cut -c1-200 "$WORK/dev.raw" | sed 's/^/  /'

# --- The floor: a MEASURED minimum, and the guard against a green run over ---
#     nothing. This script exits on $FAIL alone, and $FAIL is 0 both when every
#     assertion passed and when NO assertion ran. The hazard is sharp here: the
#     headline verdict is "the deployed body ships NO stack", which an EMPTY
#     body satisfies perfectly. `err.ok` is the control for that on one row; the
#     floor is the control for the run as a whole. This repo has shipped three
#     gates that passed over zero tests (#102/#103/#112).
#
# THE FLOOR IS A MEASUREMENT. Taken 2026-08-10 on this tree, running this script
# unmodified:
#
#     errors dev vs deployed: 24 passed, 0 failed, 0 leaks        (exit 0)
#
# CROSS-CHECKED against a second, independent instrument: CALL SITES in the
# source, with loops multiplied out. 12 unconditional top-level `pass` sites
# (AUTH_INSECURE_DEV unset here, .zship built, leak marker matches the fixture,
# all 5 procedures anon, dev reachable, dev probe, dev-vs-dev self-diff empty,
# deployed, the live-process rail check, gateway routes, deployed probe, the
# err.ok CONTROL) + 4 from the `for p in err.plain err.status4xx
# err.status4xxCode err.publicCode5xx` no-stack loop + 1 section-5 diff = 17,
# PLUS the 7 `_stk_ok` sites in the SHARED tests/lib/e2e_stack.sh (PG, init.sql,
# migrations, control, worker, gateway, pat+jwt), which increment the same
# counter and are why counting `pass "` in this file alone under-counts by
# exactly 7. 17 + 7 = 24. Dynamic and static agree, and they fail differently.
#
# NO HEADROOM: the total is fixed by the source, not discovered at run time.
#
# WHAT THE FLOOR DOES NOT CATCH: substitution. Swapping one assertion for an
# easier one keeps the total at 24. Nothing here can see that; review can.
ERRORS_MIN_PASSED="${ERRORS_MIN_PASSED:-24}"

echo ""
echo "  errors dev vs deployed: $PASS passed, $FAIL failed, $leaks deployed rows leaking a stack  (floor $ERRORS_MIN_PASSED)"

rc=0
[ "$FAIL" -eq 0 ] || rc=1
if [ "$PASS" -lt "$ERRORS_MIN_PASSED" ]; then
  echo "FAIL: only $PASS assertions passed, fewer than the $ERRORS_MIN_PASSED this gate expects." >&2
  echo "      Assertions do not vanish by accident, and 'no stack in the body' is a" >&2
  echo "      verdict an EMPTY body also satisfies -- so a shrinking count is exactly" >&2
  echo "      the shape a silently-broken run takes here. If an assertion was removed" >&2
  echo "      deliberately, lower ERRORS_MIN_PASSED in the same change and say why." >&2
  rc=1
fi
exit "$rc"
