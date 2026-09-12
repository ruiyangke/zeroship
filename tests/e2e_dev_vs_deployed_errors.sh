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
#   Symmetrically, a crate-local suite reads the envelope from ONE side only:
#   `crates/zeroship-data-v8/tests/capability.rs` can read a verbose `code`/`details`
#   body because `CAPABILITY_VIOLATION` is on `is_public_error_code`'s
#   allow-list, so it never exercises the blanking arm this harness measures.
#
# THE FIXTURE (examples/error-probe) HOLDS EVERYTHING CONSTANT BUT ONE THING.
#   All four throwing procedures share one throw site and one message, so rows
#   differ only in which properties ride on the thrown error:
#
#     err.plain          {}                          -> 500, sanitizer's blank arm
#     err.status4xx      {status:403}                -> one variable vs err.plain
#     err.status4xxCode  {status:403,code:...}       -> one variable vs err.status4xx
#     err.publicCode5xx  {code:"concurrency_mismatch"}   -> one variable vs err.plain,
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
#   2. THE RELATIVE HALF HAS NO ONE-SIDED LEVER ANY MORE, and that is a real
#      gap, not an omission. It used to be MUTATE=dev-insecure, which set
#      AUTH_INSECURE_DEV=true on the dev child only and drove the diff RED on
#      the 5xx rows. That env var was the runtime's escape hatch out of the 5xx
#      sanitization rail; it was DELETED so the rail runs unconditionally, and
#      with it went the only way to make one tier verbose without editing the
#      platform. Nothing here re-introduces one: a lever that relaxes a security
#      rail for a test is the shape this change removed. So the relative diff is
#      currently guarded by (1) alone, and the ABSOLUTE assertions below -- which
#      read the raw deployed bytes and do not depend on the diff at all -- are
#      what actually carry the leak verdict. Read a green relative diff as
#      "no divergence observed", not as "the diff would have caught one".
#   3. MUTATE=drop-status, which removes `status: 403` from the shared source so
#      err.status4xx lands at 500 on BOTH sides. The relative diff stays green,
#      and the row is RE-JUDGED UNDER THE NEW STATUS CLASS. That is the
#      row-level sensitivity control for the absolute half -- it proves the leak
#      verdict tracks the status class rather than being hardcoded per procedure
#      name.
#
#      WHAT TO LOOK FOR, and read this before concluding the control is broken.
#      The observable is the ROW LABEL, not the pass/fail totals. Measured
#      2026-08-11, both modes run by me on the same HEAD:
#
#        MUTATE=none         ok deployed err.status4xx (HTTP 403) ships NO "stack"
#        MUTATE=drop-status  ok deployed err.status4xx (HTTP 500) ships NO "stack"
#
#      403 -> 500 is the control firing: the harness read the ACTUAL status and
#      re-derived the verdict from it. Both totals are `24 passed, 0 failed,
#      0 deployed rows leaking a stack` and both exit 0, because the platform
#      correctly ships no stack on the 500 either -- that is the platform being
#      right, not the control being inert.
#
#      THIS LINE USED TO SAY "the ABSOLUTE verdict for that row must flip",
#      which is false and actively misleading: nothing flips to FAIL, so anyone
#      running the control as documented sees a green and concludes the control
#      proves nothing. I nearly filed exactly that. Diff the ROWS between the
#      two modes, not the verdict lines -- the totals are equal by design.
#
# ===========================================================================
# SECOND LEG, added 2026-08-11: DISPATCHER-ORIGINATED errors.
#
# Everything above is about errors USER CODE throws. This harness now also
# drives the errors produced BEFORE user code runs. WHY THAT IS A DIFFERENT
# QUESTION rather than more rows of the same one: the rail forks by WHICH ERROR
# it is, not by which tier you are on.
#
#   SHARED. Both tiers run the same Rust runtime -- `pnpm dev` is
#   `target/release/zeroship`, the deploy is `zeroship-worker` -- so the input
#   parser (`crates/zeroship-runtime/src/core/runtime.rs::parse_rpc_input`) and the
#   dispatch body (`__zsDispatchRpc`, inline in `crates/runtime/src/core/
#   init.rs`) are the same code on both. MEASURED, not assumed: d.badJson
#   answers with the RUST parser's text ("invalid JSON body") on both, where
#   the JS parser in `sdks/bootstrap/src/fetch-handler.ts` would have appended
#   the underlying parse error.
#
#   DEV ONLY. `fetch-handler.ts` still owns the cases the kernel's
#   `extract_zs_v1_id` declines to claim: a non-GET/POST method, and a path
#   with no id. In dev nothing is in front of it, so it answers them.
#
#   DEPLOYED ONLY. The gateway sits in front and answers anything it can settle
#   from the manifest without asking the worker: an unknown procedure id, a
#   method the procedure kind forbids, a path with no id. Those three are
#   precisely where this leg found divergence, and all three were the gateway
#   speaking a different error envelope. Fixed 2026-08-11; the rows below are
#   what keeps it fixed.
#
# NOT the rail, despite its own header having said so until 2026-08-11:
# `sdks/bootstrap/src/dispatcher.ts`'s `__zsDispatch`. `dist/dispatcher.js` is
# not `include_str!`d anywhere in `crates/runtime` (grep it), and the unary path
# never reaches that function on either tier -- the synthetic entry exports
# `default.rpc` as a dict, the kernel wraps it in `USER_RPC`, and that calls
# init.rs's second copy. The two copies have ALREADY drifted in text ("No RPC
# dispatch table installed" vs "RPC registry is not an object"); it is not
# client-visible only because that arm is unreachable in a built app.
#
# THE CASES, and the ONE variable each moves:
#
#   d.goodInput     POST err.needsInput {"must":"<marker>"}  the CONTROL: 200 + marker
#   d.wrongField    POST err.needsInput {"WRONGFIELD":1}     control + field name
#   d.emptyBody     POST err.needsInput (no body)            control + body removed
#   d.unknownProc   POST err.doesNotExist                    control + procedure id
#   d.badJson       POST err.ok  <non-JSON bytes>            control + body syntax
#   d.badBase64Get  GET  err.ok?input=!!!!                   badJson, on the GET rail
#   d.wrongMethod   PUT  err.ok  {"json":{}}                 control + HTTP method
#   d.emptyId       POST /__zeroship/v1/  (no id at all)     control + id removed
#
# d.goodInput is the row that makes the other seven mean anything. "the body
# carries no stack", "the status is not 200", "the marker is absent" are all
# satisfied by a request that never reached the dispatcher - a gateway 404, an
# unrouted app, a dead worker. Only the control separates a REJECTION from an
# ABSENCE, and it moves exactly one variable away from d.wrongField.
#
# WHAT IS ABSOLUTE AND WHAT IS RELATIVE, stated per row rather than in general,
# because "the diff is green" is worthless when both tiers are broken the same
# way (see "What a dev-vs-deployed comparison cannot see" in
# docs/pilot/e2e-scenarios.md):
#
#   RELATIVE (the diff, section 7): every row, dev vs deployed, scrubbed.
#   ABSOLUTE (section 6, on the RAW deployed bytes only):
#     A1  no dispatcher-originated deployed body carries a "stack" key
#     A2  no dispatcher-originated deployed body carries an absolute filesystem
#         path or a node_modules specifier
#     A3  d.wrongMethod did NOT execute the procedure (marker absent), paired
#         against d.goodInput, which proves the marker DOES travel this transport
#     A4  d.unknownProc is 4xx - not 200, not 5xx
#     A5  d.wrongField and d.emptyBody are 400 AND carry code INVALID_ARGUMENT
#     A6  d.goodInput is 200 and carries the marker
#     A7  d.badJson does not REFLECT the malformed request bytes back to the
#         caller (a distinctive sentinel is planted in the body for this)
#
#   Each of A1-A7 would still fire if BOTH tiers were broken identically, which
#   is the property the relative half cannot have.
#
# WHAT IS SCRUBBED, AND WHY IT IS NOT ERASING THE EVIDENCE. The dispatcher rows
# reuse the same scrub as the throw rows: `stack` value, `request_id` value,
# and bare UUIDs. Those three are per-request or per-machine by construction and
# can never be a contract. Critically the scrub collapses the VALUE and keeps the
# KEY, so `"stack":"<STACK>"` present on one tier and absent on the other still
# shows up in the diff - and the leak question is answered on the RAW file
# anyway (A1), never on the scrubbed one. The error MESSAGE text is deliberately
# NOT scrubbed: the message is the contract under examination here, and scrubbing
# it would delete the divergence this leg exists to find.
# ===========================================================================
#
# Prereqs (docs/runbooks/local-dev.md):
#   pnpm build
#   cargo build --release -p zeroship-control -p zeroship-worker -p zeroship-gateway -p zeroship-cli --bins
#   pnpm install && pnpm build
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
export ZEROSHIP_CONTROL_PORT="${ZEROSHIP_CONTROL_PORT:-9399}"
export ZEROSHIP_WORKER_PORT="${ZEROSHIP_WORKER_PORT:-8399}"
export ZEROSHIP_GATEWAY_PORT="${ZEROSHIP_GATEWAY_PORT:-8309}"
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

# The DISPATCHER leg. `name|method|path-after-/__zeroship/v1/|body`.
#
# BADJSON_SENTINEL rides inside the malformed body so the reflection assertion
# (A7) has something specific to look for. If a tier echoes the bytes it failed
# to parse, this string comes back.
BADJSON_SENTINEL="ZSBADJSON-9c2e-sentinel"
DCASES=(
  "d.goodInput|POST|err.needsInput|{\"json\":{\"must\":\"$LEAK_MARKER\"}}"
  "d.wrongField|POST|err.needsInput|{\"json\":{\"WRONGFIELD\":1}}"
  "d.emptyBody|POST|err.needsInput|"
  "d.unknownProc|POST|err.doesNotExist|{\"json\":{}}"
  "d.badJson|POST|err.ok|not json at all $BADJSON_SENTINEL {"
  "d.badBase64Get|GET|err.ok?input=!!!!|"
  "d.wrongMethod|PUT|err.ok|{\"json\":{}}"
  "d.emptyId|POST||{\"json\":{}}"
)
# Rows whose deployed body must never carry a stack or a filesystem path (A1/A2)
# -- i.e. everything except the control, which legitimately carries the marker
# and nothing else.
DERR_ROWS="d.wrongField d.emptyBody d.unknownProc d.badJson d.badBase64Get d.wrongMethod d.emptyId"

PASS=0; FAIL=0; PIDS=()
# Counters the summary line reads. Initialised here so an early exit cannot trip
# `set -u` on the way out, and so a run that never reaches section 6 reports 0
# leaks rather than nothing -- the RAN floor is what catches that run, not these.
leaks=0; dleaks=0; dpaths=0; D_CONTROL_OK=0
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
  "crates/zeroship-runtime/src crates/zeroship-worker/src crates/zeroship-gateway/src crates/zeroship-control/src crates/zeroship-core/src sdks/bootstrap/src sdks/rpc/src" \
  "zeroship zeroship-worker zeroship-gate zeroship-control" \
  || { _zs_fresh_rc=$?; [ "$_zs_fresh_rc" -ne 0 ] && exit "$_zs_fresh_rc"; }

echo "=== error envelopes: dev vs deployed (error-probe) ==="
echo "  mutation: $MUTATE"

# --- 0b. THE PRECONDITION THE WHOLE MEASUREMENT RESTS ON --------------------
# The 5xx sanitization rail must have NO env escape hatch. It used to have one:
# `AUTH_INSECURE_DEV`, read by
# `crates/zeroship-runtime/src/core/dispatch.rs::expose_internal_dispatch_errors` and
# `sdks/bootstrap/src/fetch-handler.ts::insecureDevErrorsEnabled`. Both readers
# are deleted. While they existed, a stray value in the environment reached BOTH
# tiers and every 5xx row below measured the hatch instead of the rail --
# silently, and in the direction that manufactures a finding.
#
# The old check here read the harness's own environment. That is now unfalsifiable
# (an unset variable nothing reads), so it checks the SOURCE instead: if a reader
# is ever re-introduced, this fires whether or not the variable happens to be set
# in the shell that runs the harness.
_ZS_HATCH_HITS="$(grep -rlE 'AUTH_INSECURE_DEV|insecureDevErrorsEnabled|expose_internal_dispatch_errors' \
  "$ROOT/crates/zeroship-runtime/src" "$ROOT/crates/zeroship-worker/src" "$ROOT/crates/zeroship-gateway/src" \
  "$ROOT/sdks/bootstrap/src" "$ROOT/sdks/rpc/src" 2>/dev/null || true)"
if [ -n "$_ZS_HATCH_HITS" ]; then
  fail "an escape hatch out of the 5xx sanitization rail is back in the source: $(tr '\n' ' ' <<<"$_ZS_HATCH_HITS")"
  exit 2
fi
pass "no source reader of a 5xx-sanitization escape hatch (the rail is unconditional)"

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
# The DISPATCHER probe. Same two-output contract as `probe`: scrubbed rows on
# stdout for the diff, verbatim rows appended to $DRAWFILE for the absolute
# assertions. Same scrub, and the same reason for each of its three rules.
# ---------------------------------------------------------------------------
dprobe() {
  local base="$1" line nm meth path body code
  local rpc="$base/__zeroship/v1"

  dscrub() {
    sed -E \
      -e 's/"stack":"[^"]*"/"stack":"<STACK>"/g' \
      -e 's/"request_id":"[^"]*"/"request_id":"<RID>"/g' \
      -e 's/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/<UUID>/g'
  }

  for line in "${DCASES[@]}"; do
    IFS='|' read -r nm meth path body <<<"$line"
    if [ "$meth" = "GET" ]; then
      code="$(curl -s -o "$WORK/dprobe.body" -w '%{http_code}' -m 20 -X GET \
                -H "Host: $HOST" "$rpc/$path")"
    else
      # --data-binary, not -d: -d strips newlines and would sanitise the very
      # malformed body d.badJson is built to send. An empty string still sets
      # Content-Length: 0, which is what d.emptyBody needs.
      code="$(curl -s -o "$WORK/dprobe.body" -w '%{http_code}' -m 20 -X "$meth" \
                -H 'content-type: application/json' -H "Host: $HOST" \
                "$rpc/$path" --data-binary "$body")"
    fi
    printf '%-15s %s %s\n' "$nm" "$code" "$(tr -d '\n' < "$WORK/dprobe.body")" >> "$DRAWFILE"
    printf '%-15s %s %s\n' "$nm" "$code" "$(tr -d '\n' < "$WORK/dprobe.body" | dscrub)"
  done
}

# `<row-name> <file>` -> the HTTP status recorded for that row.
drow_status() { awk -v n="$1" '$1==n {print $2; exit}' "$2"; }
# `<row-name> <file>` -> the body recorded for that row, VERBATIM.
# Deliberately not awk field-rebuilding: that re-joins on a single space and
# would silently rewrite any run of spaces inside the body -- which is exactly
# what the malformed-body row is made of.
drow_body()   { grep -m1 -- "^$1 " "$2" | sed -E 's/^[^ ]+ +[0-9]+ ?//'; }

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

if [ "$MUTATE" = "weak-schema" ]; then
  # THE CONTROL FOR THE ABSOLUTE HALF OF THE DISPATCHER LEG, and the one that
  # demonstrates the whole reason section 6 exists.
  #
  # Loosen err.needsInput's schema so `{"WRONGFIELD":1}` VALIDATES: drop
  # `.strict()` (unknown keys are stripped, not rejected) and make `must`
  # optional (nothing is missing). ONE variable: the schema. Both tiers rebuild
  # from the same source, so BOTH now answer d.wrongField 200 -- the input
  # validation a creator declared is silently not happening, in dev AND in
  # production.
  #
  # WHAT TO LOOK FOR: the section-7 DIFF STAYS GREEN. The tiers agree perfectly;
  # they are just both wrong. Only A5, which reads the deployed status and code
  # absolutely, turns red. If a future edit ever makes A5 relative, this mode is
  # what catches it.
  MUTATE_BAK="$WORK/index.ts.bak"
  cp "$APP/src/index.ts" "$MUTATE_BAK"
  sed -i 's|input: z.object({ must: z.string().min(1) }).strict(),|input: z.object({ must: z.string().min(1).optional() }),|' "$APP/src/index.ts"
  grep -q 'input: z.object({ must: z.string().min(1).optional() }),' "$APP/src/index.ts" \
    && echo "  MUTATED: err.needsInput's schema now ACCEPTS {\"WRONGFIELD\":1} (both builds)" \
    || { fail "weak-schema mutation did not apply"; exit 1; }
fi

( cd "$APP" && pnpm build ) > "$WORK/build.log" 2>&1
[ -f "$ZSHIP" ] && pass "built app.zship ($(du -k "$ZSHIP" | cut -f1)KB)" \
  || { fail "build produced no app.zship"; tail -30 "$WORK/build.log"; exit 1; }

# The marker in this script MUST equal the one the app throws, or every
# assertion keyed on it is vacuously green.
grep -qF "\"$LEAK_MARKER\"" "$APP/src/index.ts" \
  && pass "leak marker '$LEAK_MARKER' matches examples/error-probe/src/index.ts" \
  || { fail "marker drift: '$LEAK_MARKER' is not in $APP/src/index.ts"; exit 1; }

# EVERY procedure must be anonymous in the BUILT manifest. If any is gated, the
# gateway answers it and the deployed row is the GATEWAY's envelope, not the
# worker's -- which is precisely the blind spot this harness exists to close,
# and it would read as a clean "no stack" rather than as "never measured".
d="$WORK/unpack"; mkdir -p "$d"; tar -xf "$ZSHIP" -C "$d"
anon_ok=1
for p in $PROCS err.needsInput; do
  grep -qE "\"rpc:${p//./\\.}\":\{[^}]*\"auth\":\"anonymous\"" "$d/manifest.json" \
    || { fail "$p is not anonymous in the manifest -- deployed calls never reach the worker for it"; anon_ok=0; }
done
[ "$anon_ok" = "1" ] && pass "all $(echo $PROCS err.needsInput | wc -w) procedures are anonymous in the manifest (deployed calls reach the WORKER)"

# The dispatcher leg's INVALID_ARGUMENT rows are only reachable if the built
# manifest says err.needsInput is a `query`. If the vite plugin ever stopped
# emitting the input schema into the server bundle, `.parse()` would never run
# and d.wrongField would answer 200 -- a row that reads as a divergence when it
# is a build regression. Assert the declaration exists in the artifact.
grep -q '"rpc:err.needsInput"' "$d/manifest.json" \
  && pass "err.needsInput is declared in the built manifest (the schema-rejection rail is reachable deployed)" \
  || fail "err.needsInput is missing from the built manifest -- the dispatcher INVALID_ARGUMENT rows cannot be measured"

# ---------------------------------------------------------------------------
# 2. Dev side. `pnpm dev` spawns `zeroship serve`; the runtime's
#    `build_error_body` lives in that child.
# ---------------------------------------------------------------------------
for _p in "$DEV_PORT" "$VITE_PORT"; do
  lsof -ti :"$_p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
done
# No per-side environment is applied. The dev child used to be able to take a
# 5xx-verbosity escape hatch the deployed side did not (MUTATE=dev-insecure);
# that hatch is deleted, so both tiers now run the same rail with the same env.
( cd "$APP" && ./node_modules/.bin/vite --port "$VITE_PORT" --strictPort > "$WORK/dev.log" 2>&1 ) & PIDS+=($!)
# Readiness: a deadline plus a log-derived diagnosis, not a fixed 25 x 2s count
# sized on an idle machine (#273). This harness already sources e2e_stack.sh
# above, after its own port block, so the helper is in scope here.
_dev_ping() {
  curl -sf -o /dev/null -m 5 -X POST -H 'content-type: application/json' \
    "http://localhost:$DEV_PORT/__zeroship/v1/err.ok" -d '{"json":{}}'
}
if stack_wait_dev "dev app" "$WORK/dev.log" _dev_ping; then
  pass "dev app reachable on :$DEV_PORT"
else
  fail "dev app never became ready -- see the diagnosis and log tail above"
  exit 1
fi

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

# --- 2b. the DISPATCHER leg, dev side, with its own determinism control -----
DRAWFILE="$WORK/dev.draw"; : > "$DRAWFILE"
dprobe "http://localhost:$DEV_PORT" > "$WORK/dev.dtxt" 2>&1
[ "$(wc -l < "$WORK/dev.dtxt")" = "${#DCASES[@]}" ] \
  && pass "dev answered all ${#DCASES[@]} dispatcher cases" \
  || { fail "dev dispatcher probe produced $(wc -l < "$WORK/dev.dtxt") of ${#DCASES[@]} rows"; tail -20 "$WORK/dev.log"; }

# The dispatcher rows need their OWN determinism control, not the throw rows'.
# They exercise different code (input parse, method gate, resource lookup) and
# carry a per-request `request_id` on any 5xx arm, so "the throw probe is
# deterministic" says nothing about whether these are.
DRAWFILE="$WORK/dev-again.draw"; : > "$DRAWFILE"
dprobe "http://localhost:$DEV_PORT" > "$WORK/dev-again.dtxt" 2>&1
if diff -q "$WORK/dev.dtxt" "$WORK/dev-again.dtxt" >/dev/null 2>&1; then
  pass "dispatcher probe is deterministic: dev-vs-dev self-diff is empty"
else
  fail "dev dispatcher rows disagree with THEMSELVES across two runs -- instrument noise, not findings"
  diff "$WORK/dev.dtxt" "$WORK/dev-again.dtxt" | head -20
fi

# ---------------------------------------------------------------------------
# 3. Deployed side: real stack, real deploy, real gateway.
# ---------------------------------------------------------------------------
DEV_WORK="$WORK"
stack_up || { fail "stack bring-up failed"; exit 1; }   # stack_up resets $WORK
cp "$DEV_WORK/dev.txt" "$WORK/dev.txt"
cp "$DEV_WORK/dev.raw" "$WORK/dev.raw"
cp "$DEV_WORK/dev.dtxt" "$WORK/dev.dtxt"
cp "$DEV_WORK/dev.draw" "$WORK/dev.draw"
mint_creator_bearer || exit 1

APP_ID="$(deploy_zship "$APP_SLUG" "$ZSHIP")" || { fail "deploy error-probe"; exit 1; }
pass "deployed error-probe ($APP_ID)"

# THE DEPLOYED-WORKER ENVIRONMENT CHECK USED TO LIVE HERE and is deliberately
# gone, not misplaced. It read `/proc/<worker>/environ` and failed if
# `AUTH_INSECURE_DEV` was set, because that variable could switch the deployed
# tier onto the verbose 5xx body. Nothing reads that variable now, so the check
# could only ever pass -- a green that proves the harness ran, not that the
# worker is on the production rail. Section 0b asserts the property that still
# has content: no source file reads such a hatch at all.

# Wait for the gateway to pull the route before probing (an unrouted call is a
# 404/503 and would read as a divergence when it is a race).
ready=0
for _ in $(seq 1 25); do
  c="$(curl -s -o /dev/null -w '%{http_code}' -m 10 -X POST -H 'content-type: application/json' \
      -H "Host: $HOST" "http://localhost:$ZEROSHIP_GATEWAY_PORT/__zeroship/v1/err.ok" -d '{"json":{}}')"
  [ "$c" = "200" ] && { ready=1; break; }
  sleep 2
done
[ "$ready" = "1" ] && pass "gateway routes to the deployed app (err.ok -> 200)" \
  || { fail "gateway never routed to the app (last code=$c)"; tail -20 "$WORK/gate.log"; }

RAWFILE="$WORK/deployed.raw"; : > "$RAWFILE"
probe "http://localhost:$ZEROSHIP_GATEWAY_PORT" > "$WORK/deployed.txt" 2>&1
grep -q 'err.plain' "$WORK/deployed.txt" && pass "deployed app answered the probe ($(wc -l < "$WORK/deployed.txt") rows)" \
  || { fail "deployed probe produced nothing"; tail -20 "$WORK/worker.log"; }

DRAWFILE="$WORK/deployed.draw"; : > "$DRAWFILE"
dprobe "http://localhost:$ZEROSHIP_GATEWAY_PORT" > "$WORK/deployed.dtxt" 2>&1
[ "$(wc -l < "$WORK/deployed.dtxt")" = "${#DCASES[@]}" ] \
  && pass "deployed app answered all ${#DCASES[@]} dispatcher cases" \
  || { fail "deployed dispatcher probe produced $(wc -l < "$WORK/deployed.dtxt") of ${#DCASES[@]} rows"; tail -20 "$WORK/worker.log"; }

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

# ---------------------------------------------------------------------------
# 6. THE DISPATCHER LEG, ABSOLUTE HALF. Every assertion here reads the RAW
#    DEPLOYED bytes and would still fire if dev were broken the same way.
# ---------------------------------------------------------------------------
echo ""
echo "--- dispatcher-originated errors: ABSOLUTE assertions on the DEPLOYED body ---"

# A6 first: the control. Read this before believing anything below it. It is
# ONE VARIABLE away from d.wrongField (the field name), so a green on it means
# the dispatcher was reached, ran, and returned this app's own payload.
d_ctl_st="$(drow_status d.goodInput "$WORK/deployed.draw")"
d_ctl_body="$(drow_body d.goodInput "$WORK/deployed.draw")"
echo "  control  d.goodInput  $d_ctl_st  $d_ctl_body"
if [ "$d_ctl_st" = "200" ] && printf '%s' "$d_ctl_body" | grep -qF "$LEAK_MARKER"; then
  pass "A6 CONTROL: deployed d.goodInput is 200 and echoes '$LEAK_MARKER' (the dispatcher ran)"
  D_CONTROL_OK=1
else
  fail "A6 CONTROL: deployed d.goodInput did NOT return 200+marker (got $d_ctl_st) -- every verdict below is unsafe, an unreached dispatcher satisfies all of them"
  D_CONTROL_OK=0
fi

# A1: no stack, on the RAW bytes. The scrubbed rows cannot answer this.
dleaks=0
for r in $DERR_ROWS; do
  row="$(grep "^$r " "$WORK/deployed.draw")"
  if printf '%s' "$row" | grep -q '"stack":'; then
    dleaks=$((dleaks+1))
    fail "A1 LEAK: deployed $r ships a \"stack\" to the client"
    printf '        %s\n' "$(printf '%s' "$row" | cut -c1-320)"
  fi
done
[ "$dleaks" -eq 0 ] && pass "A1: none of the $(echo $DERR_ROWS | wc -w) deployed dispatcher error bodies ships a \"stack\""

# A2: no absolute filesystem path and no bundler specifier. A dev-server error
# message routinely names the file it was parsing; if any of that reaches the
# deployed wire it is an information leak the relative diff cannot see, because
# dev leaking the same class of string makes the rows differ only in content.
dpaths=0
for r in $DERR_ROWS; do
  row="$(grep "^$r " "$WORK/deployed.draw")"
  if printf '%s' "$row" | grep -qE '/home/|/root/|/nix/store/|node_modules|\.zeroship/'; then
    dpaths=$((dpaths+1))
    fail "A2 LEAK: deployed $r carries a filesystem path or module specifier"
    printf '        %s\n' "$(printf '%s' "$row" | cut -c1-320)"
  fi
done
[ "$dpaths" -eq 0 ] && pass "A2: no deployed dispatcher error body carries an absolute path or node_modules specifier"

# A3: the wrong method must not EXECUTE the procedure. Its one-variable partner
# is d.goodInput above, which proves the marker travels this exact transport --
# so "marker absent" here means "did not run", not "markers never arrive".
d_wm_st="$(drow_status d.wrongMethod "$WORK/deployed.draw")"
d_wm_body="$(drow_body d.wrongMethod "$WORK/deployed.draw")"
if printf '%s' "$d_wm_body" | grep -qF "$LEAK_MARKER"; then
  fail "A3: deployed PUT on /__zeroship/v1/err.ok EXECUTED the procedure (HTTP $d_wm_st, body carries the marker)"
else
  pass "A3: deployed PUT on /__zeroship/v1/err.ok did NOT execute the procedure (HTTP $d_wm_st, no marker)"
fi

# A4: an unknown procedure id must be a client error. 200 would mean something
# answered on the app's behalf; 5xx would mean the platform crashed on input it
# is supposed to reject.
d_up_st="$(drow_status d.unknownProc "$WORK/deployed.draw")"
case "$d_up_st" in
  4??) pass "A4: deployed unknown procedure id is HTTP $d_up_st (4xx)" ;;
  *)   fail "A4: deployed unknown procedure id answered HTTP $d_up_st, which is not 4xx" ;;
esac

# A5: the schema-rejection rail. This is the row the earlier observation was
# about ("{\"WRONGFIELD\":...} produced INVALID_ARGUMENT") -- confirmed or
# refuted here on the DEPLOYED tier specifically, on both status and code.
for r in d.wrongField d.emptyBody; do
  st="$(drow_status "$r" "$WORK/deployed.draw")"
  bd="$(drow_body "$r" "$WORK/deployed.draw")"
  if [ "$st" = "400" ] && printf '%s' "$bd" | grep -q '"code":"INVALID_ARGUMENT"'; then
    pass "A5: deployed $r is 400 with code INVALID_ARGUMENT"
  else
    fail "A5: deployed $r is HTTP $st and its body is not code=INVALID_ARGUMENT -- $bd"
  fi
done

# A7: the malformed-body 400 must not hand the caller back the bytes it could
# not parse. Reflection is how a parse error becomes an XSS or a log-injection
# primitive, and it is invisible to the diff if both tiers reflect.
d_bj_body="$(drow_body d.badJson "$WORK/deployed.draw")"
if printf '%s' "$d_bj_body" | grep -qF "$BADJSON_SENTINEL"; then
  fail "A7: deployed d.badJson REFLECTS the malformed request bytes back ('$BADJSON_SENTINEL' in the response)"
else
  pass "A7: deployed d.badJson does not reflect the malformed request bytes"
fi

if [ "$D_CONTROL_OK" != "1" ]; then
  echo "  (A1-A7 above are UNSAFE: the d.goodInput control failed)"
fi

# ---------------------------------------------------------------------------
# 7. THE DISPATCHER LEG, RELATIVE HALF. Green here means the tiers AGREE; it
#    does NOT mean either is right. Section 6 is the one that answers that.
# ---------------------------------------------------------------------------
echo ""
if diff -q "$WORK/dev.dtxt" "$WORK/deployed.dtxt" >/dev/null 2>&1; then
  pass "dev and deployed agree on every dispatcher-originated error envelope"
else
  dn=$(diff "$WORK/dev.dtxt" "$WORK/deployed.dtxt" | grep -c '^<')
  fail "dev and deployed DIVERGE on $dn of $(wc -l < "$WORK/dev.dtxt") dispatcher rows (< dev, > deployed)"
  diff "$WORK/dev.dtxt" "$WORK/deployed.dtxt"
fi

echo ""
echo "  --- raw DISPATCHER rows, dev (verbatim) ---"
cut -c1-260 "$WORK/dev.draw" | sed 's/^/  /'
echo "  --- raw DISPATCHER rows, deployed (verbatim) ---"
cut -c1-260 "$WORK/deployed.draw" | sed 's/^/  /'

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
# source, with loops multiplied out. (HISTORICAL: of the sites named below, "the
# live-process rail check" was DELETED and "AUTH_INSECURE_DEV unset here" was
# REPLACED by a source-level check -- see the 2026-08-12 note further down. Kept
# verbatim because the later accounting is stated as a delta against it.)
# 12 unconditional
# top-level `pass` sites
# (AUTH_INSECURE_DEV unset here, .zship built, leak marker matches the fixture,
# all 5 procedures anonymous, dev reachable, dev probe, dev-vs-dev self-diff empty,
# deployed, the live-process rail check, gateway routes, deployed probe, the
# err.ok CONTROL) + 4 from the `for p in err.plain err.status4xx
# err.status4xxCode err.publicCode5xx` no-stack loop + 1 section-5 diff = 17,
# PLUS the 7 `_stk_ok` sites in the SHARED tests/lib/e2e_stack.sh (PG, init.sql,
# migrations, control, worker, gateway, at+jwt), which increment the same
# counter and are why counting `pass "` in this file alone under-counts by
# exactly 7. 17 + 7 = 24. Dynamic and static agree, and they fail differently.
#
# NO HEADROOM: the total is fixed by the source, not discovered at run time.
#
# WHAT THE FLOOR DOES NOT CATCH: substitution. Swapping one assertion for an
# easier one keeps the total at 24. Nothing here can see that; review can.
# THE FLOOR NOW COUNTS ASSERTIONS THAT *RAN*, NOT THAT PASSED, and the change
# is not cosmetic. `PASS` alone is not a floor: mutate the product, an outcome
# moves from the PASS column to the FAIL column, and the floor trips for a
# reason that has nothing to do with coverage -- so every legitimate red also
# reads as "assertions went missing", and the one signal the floor exists to
# give (an assertion was DELETED or never reached) is drowned. RAN = PASS+FAIL
# is invariant under any mutation of the product and moves only when an
# assertion is genuinely lost. `FAIL -ne 0` already fails the run on its own,
# so nothing is weakened by dropping the PASS floor.
#
# THE FLOOR IS A MEASUREMENT, re-taken 2026-08-11 on this tree after the
# dispatcher leg landed. See the report for the run this came from.
#
# 24 (throw leg, cross-checked by call site in the note this replaced: 12
# top-level + 4 no-stack loop + 1 section-5 diff + 7 `_stk_ok` sites in the
# shared tests/lib/e2e_stack.sh) + 13 dispatcher-leg sites:
#   err.needsInput in manifest, dev answered N cases, dispatcher determinism,
#   deployed answered N cases, A6 control, A1, A2, A3, A4, A5 x2, A7,
#   section-7 diff
# = 37. Dynamic and static agree, and they fail differently.
#
# LOWERED TO 36 ON 2026-08-12, BY CALL SITE ONLY -- the run was NOT re-taken.
# One top-level site was deleted: the `/proc/<worker>/environ` check for
# `AUTH_INSECURE_DEV`. That variable's readers are gone, so the check could only
# ever pass. 12 top-level sites became 11; every other term is untouched, so
# 37 - 1 = 36. If a real run reports anything other than 36 ran, trust the run
# and fix this note, not the other way round.
#
# WHAT THE FLOOR DOES NOT CATCH: substitution. Swapping one assertion for an
# easier one keeps the total at 36. Nothing here can see that; review can.
ERRORS_MIN_RAN=36
RAN=$((PASS+FAIL))

echo ""
echo "  errors dev vs deployed: $PASS passed, $FAIL failed, $RAN ran, $leaks throw rows + $dleaks dispatcher rows leaking a stack  (ran floor $ERRORS_MIN_RAN)"

rc=0
[ "$FAIL" -eq 0 ] || rc=1
if [ "$RAN" -lt "$ERRORS_MIN_RAN" ]; then
  echo "FAIL: only $RAN assertions RAN, fewer than the $ERRORS_MIN_RAN this gate expects." >&2
  echo "      Assertions do not vanish by accident, and 'no stack in the body' is a" >&2
  echo "      verdict an EMPTY body also satisfies -- so a shrinking count is exactly" >&2
  echo "      the shape a silently-broken run takes here. A mutated product moves an" >&2
  echo "      outcome between passed and failed and leaves this number alone; only a" >&2
  echo "      LOST assertion drops it. If an assertion was removed deliberately, lower" >&2
  echo "      ERRORS_MIN_RAN in the same change and say why." >&2
  rc=1
fi
exit "$rc"
