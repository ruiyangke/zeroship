#!/usr/bin/env bash
# ============================================================================
# tests/e2e_gateway_path_backslash.sh
#
# THE QUESTION: can a request path carrying a LITERAL BACKSLASH (0x5C) make
# the gateway authorize one resource while the worker serves a different one?
#
# Why this cannot be a crate-local test. The defect class is a DISAGREEMENT
# between two components that are each self-consistent:
#
#   * the gateway splits a request path on '/' only, matches the resulting
#     segments against `manifest.resources`, and enforces that resource's
#     `auth` level (crates/zeroship-bundle/src/compiled.rs, router/dispatch.rs);
#   * the worker hands the forwarded URL string to the V8 runtime, whose
#     WHATWG/ada URL parser folds '\' -> '/' for special schemes before the
#     app ever sees `new URL(request.url).pathname`.
#
# `crates/zeroship-bundle/src/compiled.rs` states that dot-segments and '//' are
# "exactly the forms a browser's WHATWG `new URL` rewrites" and rejects them
# (400). A backslash run is a THIRD such form and is not modelled. A unit test
# on either side passes; only a request driven end to end through a running
# gateway into a deployed app can see the seam.
#
# WHAT IS ASSERTED, and the one-variable controls that make each assertion mean
# something:
#
#   T1  liveness       GET /pub/hello          -> 200, worker pathname /pub/hello
#   T2  the gate       GET /admin/secret       -> 401/403 (auth:user resource)
#                      T2 is T4's one-variable partner: identical app, identical
#                      deploy, identical target resource. Without it a 200 on T4
#                      could just mean "nothing is gated here".
#   T3  SEC-2 guard    GET /pub/../admin/secret (--path-as-is) -> 400
#                      Proves the existing dot-segment guard is live in THIS
#                      binary, so a T4 that is NOT rejected is a gap in the
#                      guard's coverage rather than a guard that never ran.
#   T4  the question   GET /pub\..\admin/secret (raw socket, literal 0x5C)
#   T5  simpler form   GET /\admin/secret       (raw socket, literal 0x5C)
#                      No '..' at all: one leading backslash is enough to make
#                      the gateway see the segment "\admin" while WHATWG folds
#                      it to "/admin/secret".
#   T6  encoding ctl   GET /pub%5c..%5cadmin/secret -> %5c must NOT decode.
#                      Distinguishes "the byte matters" from "any spelling of a
#                      backslash matters".
#
# The decisive artefact is the pair (gateway authorization decision, worker
# pathname). The gateway's decision is observable as the HTTP status: a 401/403
# means it matched the auth:user resource; a 200 means it authorized the request
# as anonymous. The worker's view is echoed by the fixture app itself, which
# routes on `new URL(request.url).pathname` exactly as a real app does.
#
# Exit non-zero if any assertion fails. RESULT lines are counted and summed, not
# tailed.
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 1

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); echo "  RESULT ok   $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  RESULT FAIL $1"; }
note() { echo "  ---- $1"; }

# --- binary freshness -------------------------------------------------------
# A gateway binary older than crates/gateway/src reads as a real divergence.
# BASH ONLY (the lists rely on word splitting); see tests/lib/binary_freshness.sh.
# shellcheck source=tests/lib/binary_freshness.sh
source "$ROOT/tests/lib/binary_freshness.sh"
zs_check_binary_freshness "$ROOT" "$ROOT/target/release" \
  "crates/zeroship-gateway/src crates/zeroship-worker/src crates/zeroship-runtime/src crates/zeroship-bundle/src crates/zeroship-control/src" \
  "zeroship zeroship-worker zeroship-gate zeroship-control"
FRESH_RC=$?
if [ "$FRESH_RC" = "2" ]; then
  echo "FRESHNESS could not answer (rc=2) -- refusing to report a result over unknown binaries." >&2
  exit 2
fi

# --- stack ------------------------------------------------------------------
# PER-RUN ports and a PER-RUN container name. Fixed constants are shared state:
# a second run of THIS harness collides with the first on every one, and a fixed
# Postgres port also collides with tests/e2e_metering_billing.sh. Worse, fixed
# constants force `stack_up` to open by SIGKILLing whatever held them, which on
# this box means a peer agent's server. Allocated ports have nothing to reclaim,
# so nothing is killed.
#
# `stack_up` reads and re-exports these, and every use of them in this file is
# AFTER `stack_up` returns, so the allocated values are what the requests go to.
# shellcheck source=tests/lib/e2e_ports.sh
source "$ROOT/tests/lib/e2e_ports.sh"
zs_ports_reserve ZEROSHIP_CONTROL_PORT ZEROSHIP_WORKER_PORT ZEROSHIP_GATEWAY_PORT PG_PORT || exit 1
export PG_CONTAINER="zs-e2e-bslash-pg-$$"
# Debug logging on the gateway so the matched path is recoverable from gate.log
# when a status alone is ambiguous.
export ZEROSHIP_OBSERVABILITY_LOG_FILTER=${ZEROSHIP_OBSERVABILITY_LOG_FILTER:-info,zeroship_gateway=debug}

# shellcheck source=tests/lib/e2e_stack.sh
source "$ROOT/tests/lib/e2e_stack.sh"

cleanup() { stack_down; zs_ports_release; }
trap cleanup EXIT

echo "=== bring-up ==="
stack_up || { echo "stack_up failed"; exit 1; }
mint_creator_bearer || { echo "mint_creator_bearer failed"; exit 1; }

# --- fixture ----------------------------------------------------------------
# The app routes on `new URL(request.url).pathname` -- the same thing every
# real fetch handler, SPA server and hand-rolled router does. `served` is the
# resource the WORKER decided to serve; `pathname` is the worker's view of the
# path; `rawUrl` is the URL string the gateway forwarded (post-WHATWG-parse, so
# it shows what the parser made of the bytes).
APPJS="$WORK/app.js"
cat > "$APPJS" <<'JS'
export default {
  // `rpc` is the platform-standard surface (`default = { fetch?, rpc? }`).
  // Native runtime dispatch routes
  // `/__zeroship/v1/<id>` from `new URL(request.url).pathname`, so T7 exercises
  // the platform's routing, not this fixture's.
  rpc: {
    secret() {
      return { served: "RPC-SECRET-DATA" };
    },
  },
  fetch(request) {
    const u = new URL(request.url);
    const p = u.pathname;
    const served = p === "/admin/secret" ? "ADMIN-SECRET-DATA" : "public";
    return new Response(
      JSON.stringify({ rawUrl: request.url, pathname: p, served }),
      { headers: { "content-type": "application/json" } },
    );
  },
};
JS

build_zship() {
  local js_file="$1" out_path="$2" resources="$3"
  local stage; stage=$(mktemp -d -t zs-bslash-zship-XXXXXX)
  mkdir -p "$stage/blobs"
  local hash; hash=$(sha256sum "$js_file" | awk '{print $1}')
  cp "$js_file" "$stage/blobs/$hash"
  local now; now=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
  cat > "$stage/manifest.json" <<EOF
{"version":1,"resources":$resources,"assets":{},"runtime_assets":{},"asset_version":0,"sourcemaps":{},"worker":{"entry":"index.js","modules":{"index.js":"$hash"}},"metadata":{"compiler":"e2e-bslash","built_at":"$now"}}
EOF
  (cd "$stage" && tar --format=ustar -cf - manifest.json "blobs/$hash") | zstd -q -f -o "$out_path"
  rm -rf "$stage"
}

# The arrangement under test: a public catch-all (an SPA/SSR fallback) plus a
# gated admin subtree. This is the ordinary shape, not a contrived one.
RESOURCES='{"/[...rest]":{"auth":"anonymous","publicly_accessible":true},"/admin/[...rest]":{"auth":"user"},"rpc:secret":{"auth":"user"}}'
ZSHIP="$WORK/bslash.zship"
build_zship "$APPJS" "$ZSHIP" "$RESOURCES"

SLUG="bslash"
APP_ID="$(deploy_zship "$SLUG" "$ZSHIP")" || { echo "deploy failed"; exit 1; }
note "deployed app $APP_ID as /apps/$SLUG"
sleep 6   # route sync poll-interval is 2s; give the gateway room.

BASE="http://localhost:$ZEROSHIP_GATEWAY_PORT/apps/$SLUG"

# --- request helpers --------------------------------------------------------
# curl_status_body <extra curl args...> <url>  -> sets RSTATUS / RBODY
curl_req() {
  local raw
  raw="$(curl -sS -m 30 -w $'\n%{http_code}' "$@" 2>&1)"
  RSTATUS="${raw##*$'\n'}"
  RBODY="${raw%$'\n'*}"
}

# raw_req <path> -> sets RSTATUS / RBODY, sending <path> byte-for-byte on a raw
# socket. No curl, no URL library: the literal 0x5C reaches the wire unmodified.
# BLOCKERS ARE NOT WORKED AROUND -- if the byte cannot get onto the wire this
# helper reports it rather than substituting something curl finds acceptable.
raw_req() {
  local path="$1" resp
  exec 3<>"/dev/tcp/127.0.0.1/$ZEROSHIP_GATEWAY_PORT" || { RSTATUS="000"; RBODY="connect failed"; return 1; }
  printf 'GET %s HTTP/1.1\r\nHost: localhost:%s\r\nConnection: close\r\n\r\n' \
    "$path" "$ZEROSHIP_GATEWAY_PORT" >&3
  resp="$(timeout 30 cat <&3)"
  exec 3<&- 2>/dev/null
  exec 3>&- 2>/dev/null
  RSTATUS="$(printf '%s' "$resp" | head -1 | awk '{print $2}')"
  RBODY="$(printf '%s' "$resp" | awk 'BEGIN{b=0} /^\r?$/{if(!b){b=1;next}} b{print}')"
  [ -n "$RSTATUS" ] || RSTATUS="000"
  return 0
}

jf() { printf '%s' "$1" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{process.stdout.write(String(JSON.parse(s)[process.argv[1]]??""))}catch(e){process.stdout.write("")}})' "$2"; }

echo ""
echo "=== T1: liveness (public path served, worker echoes its own pathname) ==="
curl_req "$BASE/pub/hello"
note "T1 status=$RSTATUS body=$(printf '%s' "$RBODY" | cut -c1-200)"
if [ "$RSTATUS" = "200" ] && [ "$(jf "$RBODY" pathname)" = "/pub/hello" ]; then
  pass "T1 public path 200 and worker pathname /pub/hello"
else
  fail "T1 expected 200 + pathname /pub/hello, got $RSTATUS $(printf '%s' "$RBODY" | cut -c1-200)"
fi

echo ""
echo "=== T2: the gate (auth:user resource rejects an anonymous request) ==="
curl_req "$BASE/admin/secret"
note "T2 status=$RSTATUS body=$(printf '%s' "$RBODY" | cut -c1-200)"
if [ "$RSTATUS" = "401" ] || [ "$RSTATUS" = "403" ]; then
  pass "T2 /admin/secret rejected anonymously (HTTP $RSTATUS)"
else
  fail "T2 expected 401/403 for /admin/secret, got $RSTATUS $(printf '%s' "$RBODY" | cut -c1-200)"
fi

echo ""
echo "=== T3: SEC-2 dot-segment guard is live in THIS binary ==="
curl_req --path-as-is "$BASE/pub/../admin/secret"
note "T3 status=$RSTATUS body=$(printf '%s' "$RBODY" | cut -c1-200)"
if [ "$RSTATUS" = "400" ]; then
  pass "T3 dot-segment path rejected 400 (guard is running)"
elif [ "$RSTATUS" = "401" ] || [ "$RSTATUS" = "403" ]; then
  pass "T3 dot-segment path resolved to the gated resource (HTTP $RSTATUS) -- also safe"
else
  fail "T3 dot-segment path returned $RSTATUS (expected 400, or 401/403 if canonicalized to the gated resource)"
fi

echo ""
echo "=== T4: literal backslash traversal, raw socket ==="
raw_req '/apps/bslash/pub\..\admin/secret'
T4_STATUS="$RSTATUS"; T4_BODY="$RBODY"
note "T4 status=$T4_STATUS body=$(printf '%s' "$T4_BODY" | cut -c1-300)"
T4_PATH="$(jf "$T4_BODY" pathname)"
T4_SERVED="$(jf "$T4_BODY" served)"
note "T4 worker pathname='$T4_PATH' served='$T4_SERVED'"
if [ "$T4_STATUS" = "200" ] && [ "$T4_SERVED" = "ADMIN-SECRET-DATA" ]; then
  fail "T4 BYPASS: gateway authorized anonymously (200) while the worker served $T4_PATH"
elif [ "$T4_STATUS" = "401" ] || [ "$T4_STATUS" = "403" ] || [ "$T4_STATUS" = "400" ]; then
  pass "T4 backslash traversal refused by the gateway (HTTP $T4_STATUS)"
elif [ "$T4_STATUS" = "200" ]; then
  pass "T4 served 200 but the worker resolved '$T4_PATH' (not the gated resource) -- no mismatch"
else
  fail "T4 unexpected status $T4_STATUS (body: $(printf '%s' "$T4_BODY" | cut -c1-200))"
fi

echo ""
echo "=== T5: single leading backslash, raw socket (no dot-segment at all) ==="
raw_req '/apps/bslash/\admin/secret'
T5_STATUS="$RSTATUS"; T5_BODY="$RBODY"
note "T5 status=$T5_STATUS body=$(printf '%s' "$T5_BODY" | cut -c1-300)"
T5_PATH="$(jf "$T5_BODY" pathname)"
T5_SERVED="$(jf "$T5_BODY" served)"
note "T5 worker pathname='$T5_PATH' served='$T5_SERVED'"
if [ "$T5_STATUS" = "200" ] && [ "$T5_SERVED" = "ADMIN-SECRET-DATA" ]; then
  fail "T5 BYPASS: gateway authorized anonymously (200) while the worker served $T5_PATH"
elif [ "$T5_STATUS" = "401" ] || [ "$T5_STATUS" = "403" ] || [ "$T5_STATUS" = "400" ]; then
  pass "T5 leading backslash refused by the gateway (HTTP $T5_STATUS)"
elif [ "$T5_STATUS" = "200" ]; then
  pass "T5 served 200 but the worker resolved '$T5_PATH' (not the gated resource) -- no mismatch"
else
  fail "T5 unexpected status $T5_STATUS (body: $(printf '%s' "$T5_BODY" | cut -c1-200))"
fi

echo ""
echo "=== T6: %5c control -- a percent-encoded backslash must not decode ==="
raw_req '/apps/bslash/pub%5c..%5cadmin/secret'
note "T6 status=$RSTATUS body=$(printf '%s' "$RBODY" | cut -c1-300)"
T6_SERVED="$(jf "$RBODY" served)"
if [ "$T6_SERVED" = "ADMIN-SECRET-DATA" ] && [ "$RSTATUS" = "200" ]; then
  fail "T6 BYPASS via %5c: worker served admin for a percent-encoded backslash"
else
  pass "T6 %5c did not reach the gated resource (HTTP $RSTATUS, served='$T6_SERVED')"
fi

echo ""
echo "=== T7: the RPC variant -- platform routing, not app routing ==="
# T7a is the one-variable control: the SAME procedure, the SAME app, reached by
# its canonical URL. The gateway enforces `rpc:secret`'s auth:user there.
curl_req "$BASE/__zeroship/v1/secret"
note "T7a canonical status=$RSTATUS body=$(printf '%s' "$RBODY" | cut -c1-200)"
if [ "$RSTATUS" = "401" ] || [ "$RSTATUS" = "403" ]; then
  pass "T7a rpc:secret rejected anonymously on its canonical URL (HTTP $RSTATUS)"
else
  fail "T7a expected 401/403 on the canonical RPC URL, got $RSTATUS $(printf '%s' "$RBODY" | cut -c1-200)"
fi
# T7b: same procedure, backslash-folded path. The gateway's rpc_index is keyed
# on a canonical path beginning `/__zeroship/v1/`; this one does not, so it
# falls through to the anonymous URL catch-all.
raw_req '/apps/bslash/x\..\__zeroship/v1/secret'
T7_STATUS="$RSTATUS"; T7_BODY="$RBODY"
note "T7b status=$T7_STATUS body=$(printf '%s' "$T7_BODY" | cut -c1-300)"
if [ "$T7_STATUS" = "200" ] && printf '%s' "$T7_BODY" | grep -q 'RPC-SECRET-DATA'; then
  fail "T7b BYPASS: auth:user RPC procedure executed anonymously via a backslash path"
elif [ "$T7_STATUS" = "401" ] || [ "$T7_STATUS" = "403" ] || [ "$T7_STATUS" = "400" ]; then
  pass "T7b backslash RPC path refused by the gateway (HTTP $T7_STATUS)"
else
  pass "T7b did not reach the procedure (HTTP $T7_STATUS, body $(printf '%s' "$T7_BODY" | cut -c1-160))"
fi

echo ""
echo "=== T8: literal TAB (0x09) -- the other WHATWG path rewrite ==="
# WHATWG `new URL` STRIPS tab/LF/CR from the input before parsing, so
# "adm<TAB>in" reads as "admin" to the worker while the gateway sees a segment
# that matches no gated resource. Same class as the backslash, different byte.
raw_req "$(printf '/apps/bslash/adm\tin/secret')"
note "T8 status=$RSTATUS body=$(printf '%s' "$RBODY" | cut -c1-300)"
T8_SERVED="$(jf "$RBODY" served)"
if [ "$RSTATUS" = "200" ] && [ "$T8_SERVED" = "ADMIN-SECRET-DATA" ]; then
  fail "T8 BYPASS: gateway authorized anonymously (200) while the worker served $(jf "$RBODY" pathname)"
else
  pass "T8 tab path did not reach the gated resource (HTTP $RSTATUS, served='$T8_SERVED')"
fi

echo ""
echo "=== T9: RPC tag as a SUBSTRING, no backslash at all ==="
# The gateway's rpc_index fires only when the canonical path STARTS WITH
# `/__zeroship/v1/` (compiled.rs lookup_canonical_resource_key). The worker's
# RPC fast path (crates/zeroship-runtime/src/core/runtime.rs extract_zs_v1_id) does
# `url.find("/__zeroship/v1/")` -- a SUBSTRING search. Prefix vs substring is a
# gateway/worker path disagreement that needs no exotic byte at all.
curl_req "$BASE/x/__zeroship/v1/secret"
note "T9 status=$RSTATUS body=$(printf '%s' "$RBODY" | cut -c1-300)"
if [ "$RSTATUS" = "200" ] && printf '%s' "$RBODY" | grep -q 'RPC-SECRET-DATA'; then
  fail "T9 BYPASS: auth:user RPC procedure executed anonymously via /x/__zeroship/v1/secret"
else
  pass "T9 substring RPC path did not execute the procedure (HTTP $RSTATUS)"
fi

echo ""
echo "=== T10: backslash + RPC tag ==="
raw_req '/apps/bslash/pub\..\/__zeroship/v1/secret'
note "T10 status=$RSTATUS body=$(printf '%s' "$RBODY" | cut -c1-300)"
if [ "$RSTATUS" = "200" ] && printf '%s' "$RBODY" | grep -q 'RPC-SECRET-DATA'; then
  fail "T10 BYPASS: auth:user RPC procedure executed anonymously via a backslash path"
else
  pass "T10 backslash RPC path did not execute the procedure (HTTP $RSTATUS)"
fi

echo ""
echo "=== logs (gateway view of the raw requests) ==="
grep -aiE 'backslash|\\\\|admin' "$WORK/gate.log" 2>/dev/null | tail -20 | sed 's/^/  gate| /'
tail -10 "$WORK/worker.log" 2>/dev/null | sed 's/^/  wrk | /'

echo ""
echo "============================================"
echo " passed: $PASS   failed: $FAIL"
echo "============================================"
[ "$FAIL" -eq 0 ]
