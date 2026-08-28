#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# `zeroship secret expose` against a REAL control plane, and through to a
# deployed app's `process.env`.
#
# WHAT THIS COVERS THAT THE CRATE SUITE STRUCTURALLY CANNOT.
#   `PUT /api/apps/:id/env/expose` REPLACES the whole list, so the CLI's
#   `expose` does a read-modify-write: GET the current names, merge, PUT the
#   union. Sending one bare name would silently un-expose every other secret on
#   the app. That merge is unit-tested in `crates/cli/tests/secrets_test.rs`,
#   but those tests put a `curl` STUB on PATH: they prove the CLI ISSUES a GET
#   and then a PUT carrying the union. They cannot prove the control plane
#   PERSISTS that union (the PUT is answered by a shell script that prints
#   `204`), and they cannot prove a worker then hands the exposed secret to app
#   code. Everything below is read back FROM THE SERVER, or out of a response
#   produced by a deployed isolate.
#
# THE THREE THINGS ASSERTED, in increasing order of what they cost to fake:
#   1. UNION SURVIVES A REAL PUT.  `secret set A --expose` then
#      `secret set B --expose`, and the server's own list must then contain
#      BOTH. This is the row the stubbed tests cannot reach.
#   2. UNEXPOSE IS SURGICAL.  `secret unexpose D` removes D and leaves A and B.
#   3. THE POINT OF THE FEATURE.  A deployed app reads A and B from
#      `process.env` and does NOT read C or D from it.
#
# WHY THE ABSENCE HALF OF (3) NEEDS THREE SEPARATE CONTROLS. "C is not in
# process.env" is worth nothing on its own -- it is equally consistent with the
# probe being broken, the response never arriving, or C never having been
# stored. So the same response must also show:
#     transport      `fixtureMarker`, a literal compiled into the bundle.
#     the surface    a plaintext app var, and the EXPOSED secrets A and B, are
#                    readable in the very same `process.env` that withholds C
#                    and D. A and B differ from C and D in exactly one respect:
#                    whether the CLI put them on the expose list.
#     storage        C and D ARE readable on the zeroship `env` surface, which
#                    carries every stored secret regardless of the expose list.
#                    That is what separates "the opt-in gate held" from "the
#                    secret never reached the worker at all".
#
# CAN THIS FAIL? Mutate the CLI's merge, not this script:
#   crates/cli/src/secrets.rs, the `ExposeChange::Add` arm of `merge_expose`,
#   `current.iter().cloned().chain([key.to_string()]).collect()`
#     ->  `vec![key.to_string()]`
#   then `cargo build --release -p zeroship --bins`.
#
#   MEASURED 2026-08-09, not predicted: 35 passed / 0 failed becomes
#   27 passed / 8 failed. The union row reports the server's list as
#   [ZS_EXPOSED_B] rather than [ZS_EXPOSED_A ZS_EXPOSED_B]; `expose-list`, the
#   two later list rows and both `unexpose` rows follow it down (the second
#   `expose` had already dropped A, so removing D leaves the list EMPTY); and
#   the deployed `process.env` rows for A and B both go red, with the deployed
#   key list falling back to `APP_ID ZEROSHIP_DEPLOY_ID ZS_ENV_CONTROL`.
#
#   WHAT STAYED GREEN UNDER THE MUTATION, which is what makes the red readable:
#   every control. Transport, instrument, the plaintext-var positive control,
#   the storage control, and all four delivery controls. So the failure is
#   located at the merge and not at the probe, the stack, or the store.
#
# Prereqs (docs/runbooks/local-dev.md):
#   pnpm build
#   cargo build --release -p zeroship-control -p zeroship-worker -p zeroship-gateway -p zeroship --bins
#   pnpm install && pnpm build && pnpm --filter zero-migrate-cli build
#   pnpm install --filter ./examples/env-probe...
#   docker (this script starts its OWN ephemeral Postgres)
#
#   ./tests/e2e_secret_expose_cli.sh
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release"
APP="$ROOT/examples/env-probe"
ZSHIP="$APP/dist/app.zship"

# A port band of its own: golden_path 9390/8390/8300, dev-vs-deployed env
# 9397/8397/8307/5457, kv 9392/8392/8302, stream 9394/8394/8304.
export ZEROSHIP_CONTROL_PORT="${ZEROSHIP_CONTROL_PORT:-9391}"
export ZEROSHIP_WORKER_PORT="${ZEROSHIP_WORKER_PORT:-8391}"
export ZEROSHIP_GATEWAY_PORT="${ZEROSHIP_GATEWAY_PORT:-8301}"
export PG_PORT="${PG_PORT:-5451}"
export PG_CONTAINER="${PG_CONTAINER:-zs-secret-expose-pg}"
APP_SLUG="secret-expose-cli"
HOST="$APP_SLUG.localhost"

# Four secrets differing ONLY in what the CLI did with them afterwards.
SEC_A="ZS_EXPOSED_A"        # secret set --expose
SEC_B="ZS_EXPOSED_B"        # secret set --expose   (the union: A must survive)
SEC_C="ZS_STORED_ONLY"      # secret set, never exposed
SEC_D="ZS_UNEXPOSED_D"      # secret set --expose, then secret unexpose
# The plaintext var control. Must match examples/env-probe/src/index.ts's
# ZEROSHIP_CONTROL_KEY, re-asserted against that file below so the pair cannot drift.
VAR_CONTROL="ZS_ENV_CONTROL"
FIXTURE_MARKER="ZSENVP-3c9d-fixture"

# Fresh per run: a value baked into a fixture or left in a stale process can
# never satisfy an assertion by accident.
NONCE="$$-$(date +%s)-$RANDOM"
VAL_A="a-$NONCE"; VAL_B="b-$NONCE"; VAL_C="c-$NONCE"; VAL_D="d-$NONCE"
VAL_CONTROL="ctl-$NONCE"

PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }

source "$ROOT/tests/lib/e2e_stack.sh"
cleanup() {
  if [ "${KEEP_WORK:-0}" = "1" ]; then
    echo "  work dir kept: ${WORK:-<none>}"
  else
    stack_down 2>/dev/null || true
    rm -rf "${WORK_EARLY:-}"
  fi
}
trap cleanup EXIT

echo "=== zeroship secret expose: CLI -> control plane -> deployed process.env ==="

# --- 0. the CLI and the services must be the same build ---------------------
# shellcheck source=lib/binary_freshness.sh
source "$ROOT/tests/lib/binary_freshness.sh"
zs_check_binary_freshness "$ROOT" "$BIN" \
  "crates/zeroship-cli/src crates/zeroship-control/src crates/zeroship-worker/src crates/zeroship-gateway/src crates/zeroship-runtime/src" \
  "zeroship zeroship-worker zeroship-gate zeroship-control" \
  || { _zs_fresh_rc=$?; [ "$_zs_fresh_rc" -ne 0 ] && exit "$_zs_fresh_rc"; }

# PRECONDITION: the binary under test must actually HAVE these subcommands.
# `zeroship secret <unrecognised>` prints usage and exits 1, so a `zeroship`
# built before 4388f3f07 answers `secret expose` with the OLD usage block --
# and every row below then goes red for a reason that has nothing to do with
# the merge. Verified by doing it: an 18:05 source against a 12:11 binary
# produced 9 failures whose only real cause was "rebuild the CLI". Name that
# once, here, instead of nine times below.
#
# The output is captured BEFORE grepping rather than piped into it. `zeroship
# secret` with no subcommand prints usage and exits 1 BY DESIGN, and under
# `set -o pipefail` the pipeline then reports 1 even when grep matched -- so the
# piped form failed a freshly-built binary whose usage does list the
# subcommands. Verified by doing it.
usage_out="$("$BIN/zeroship" secret 2>&1)"
if printf '%s\n' "$usage_out" | grep -q "secret expose-list"; then
  pass "PRECONDITION: $BIN/zeroship carries the \`secret expose\` subcommands"
else
  fail "PRECONDITION: $BIN/zeroship has no \`secret expose\` subcommand -- rebuild with \`cargo build --release -p zeroship --bins\`. Nothing below would be about the expose list."
  exit 2
fi

# --- 1. build the probe app, and check the fixture invariants ---------------
WORK_EARLY="$(mktemp -d -t zs-secret-expose-XXXXXX)"
WORK="$WORK_EARLY"   # stack_up replaces this; the build needs scratch space now

( cd "$APP" && pnpm build ) > "$WORK/build.log" 2>&1
[ -f "$ZSHIP" ] && pass "built env-probe app.zship ($(du -k "$ZSHIP" | cut -f1)KB)" \
  || { fail "build produced no app.zship"; tail -30 "$WORK/build.log"; exit 1; }

# A name this script asserts on that the fixture does not read would make the
# assertion vacuously green.
for pair in "VAR_CONTROL:$VAR_CONTROL" "FIXTURE_MARKER:$FIXTURE_MARKER"; do
  name="${pair%%:*}"; val="${pair#*:}"
  grep -qF "\"$val\"" "$APP/src/index.ts" \
    && pass "$name '$val' matches examples/env-probe/src/index.ts" \
    || { fail "drift: '$val' is not in $APP/src/index.ts"; exit 1; }
done

# A gated procedure is answered by the GATEWAY before dispatch, so the deployed
# response would describe the gateway's refusal rather than the worker's env --
# and an env that reports nothing reads exactly like a withheld secret.
d="$WORK/unpack"; mkdir -p "$d"; tar -xf "$ZSHIP" -C "$d"
grep -qE '"rpc:envp\.report":\{[^}]*"auth":"anon"' "$d/manifest.json" \
  && pass "envp.report is anon in the manifest (deployed calls reach the WORKER)" \
  || { fail "envp.report is not anon in the manifest -- deployed calls never reach the worker"; exit 1; }

# --- 2. real stack, real bearer ------------------------------------------------
stack_up || { fail "stack bring-up failed"; exit 1; }   # stack_up resets $WORK
mint_creator_bearer || exit 1

APP_JSON="$(curl -s -X POST "http://localhost:$ZEROSHIP_CONTROL_PORT/api/apps" \
  -H 'Content-Type: application/json' -H "Authorization: Bearer $ADMIN_TOKEN" \
  -d "{\"name\":\"$APP_SLUG\"}")"
APP_ID="$(printf '%s' "$APP_JSON" | _stk_jget '.id')"
[ -n "$APP_ID" ] && pass "created app $APP_ID" || { fail "create app: $APP_JSON"; exit 1; }

# ---------------------------------------------------------------------------
# The CLI under examination, and the SERVER-SIDE read-back it is judged by.
# ---------------------------------------------------------------------------
zs() {
  "$BIN/zeroship" "$@" \
    --app="$APP_ID" --control="http://localhost:$ZEROSHIP_CONTROL_PORT" --token="$ADMIN_TOKEN"
}

# THE VERIFY STEP. Not "the CLI exited 0", and not the CLI's own stdout: an
# independent GET against the control plane, sorted, space-joined. If the PUT
# did not persist the union, this is where it shows.
server_expose() {
  curl -s -m 10 "http://localhost:$ZEROSHIP_CONTROL_PORT/api/apps/$APP_ID/env/expose" \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
  | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{
      try{const o=JSON.parse(s);if(!Array.isArray(o.expose))return process.stdout.write("<BAD-SHAPE:"+s+">");
      process.stdout.write(o.expose.slice().sort().join(" "));}
      catch(e){process.stdout.write("<UNPARSEABLE:"+s+">");}})'
}

expect_expose() {
  local want="$1" label="$2" got
  got="$(server_expose)"
  if [ "$got" = "$want" ]; then
    pass "$label -- the server's list is [$got]"
  else
    fail "$label -- the server's list is [$got], expected [$want]"
  fi
}

echo ""
echo "--- the expose list, driven by the CLI, read back from the control plane ---"

# The baseline. Without it, a later non-empty list could predate this run.
expect_expose "" "a new app exposes nothing"

out_a="$(zs secret set "$SEC_A=$VAL_A" --expose 2>&1)"
echo "$out_a" | sed 's/^/      /'
expect_expose "$SEC_A" "secret set $SEC_A --expose"

# THE UNION ROW. The PUT replaces the whole list, so a CLI that sent a bare
# single name would leave only $SEC_B here and $SEC_A would be silently
# un-exposed -- with the CLI still exiting 0 and printing a success line.
out_b="$(zs secret set "$SEC_B=$VAL_B" --expose 2>&1)"
echo "$out_b" | sed 's/^/      /'
expect_expose "$SEC_A $SEC_B" "secret set $SEC_B --expose PRESERVED $SEC_A (the union survived a real PUT)"

# The CLI's own reader, over the same server state. Both directions matter: the
# server list above is the truth, and `expose-list` is what a creator is told.
list_out="$(zs secret expose-list 2>/dev/null | sort | tr '\n' ' ' | sed 's/ $//')"
[ "$list_out" = "$SEC_A $SEC_B" ] \
  && pass "\`secret expose-list\` reports [$list_out]" \
  || fail "\`secret expose-list\` reported [$list_out], expected [$SEC_A $SEC_B]"

# Storing without --expose must not touch the list.
zs secret set "$SEC_C=$VAL_C" >/dev/null 2>&1
expect_expose "$SEC_A $SEC_B" "secret set $SEC_C (no --expose) left the list alone"

zs secret set "$SEC_D=$VAL_D" --expose >/dev/null 2>&1
expect_expose "$SEC_A $SEC_B $SEC_D" "secret set $SEC_D --expose"

# UNEXPOSE IS SURGICAL: D goes, A and B stay.
out_u="$(zs secret unexpose "$SEC_D" 2>&1)"
echo "$out_u" | sed 's/^/      /'
expect_expose "$SEC_A $SEC_B" "secret unexpose $SEC_D removed ONLY $SEC_D"

# Idempotence: a no-op change must not wipe the list (the CLI skips the PUT).
zs secret unexpose "$SEC_D" >/dev/null 2>&1
expect_expose "$SEC_A $SEC_B" "a second \`unexpose $SEC_D\` is a no-op, not a wipe"

# STORAGE CONTROL: all four secrets exist. Without this, "C is missing from
# process.env" is indistinguishable from "C was never stored".
secrets_body="$(curl -s -m 10 "http://localhost:$ZEROSHIP_CONTROL_PORT/api/apps/$APP_ID/secrets" \
  -H "Authorization: Bearer $ADMIN_TOKEN")"
missing=""
for k in "$SEC_A" "$SEC_B" "$SEC_C" "$SEC_D"; do
  printf '%s' "$secrets_body" | grep -qF "\"$k\"" || missing="$missing $k"
done
[ -z "$missing" ] \
  && pass "STORAGE CONTROL: all four secrets are stored on the app (only their expose status differs)" \
  || fail "STORAGE CONTROL: not stored:$missing -- every 'absent from process.env' verdict below would be vacuous"

# The plaintext var positive control, also through the CLI.
zs var set "$VAR_CONTROL=$VAL_CONTROL" >/dev/null 2>&1

# ---------------------------------------------------------------------------
# 3. Deploy, and ask a real isolate what it can read.
#    Secrets first, deploy second: the worker builds its isolate from the env
#    snapshot at bundle-load time.
# ---------------------------------------------------------------------------
echo ""
echo "--- the deployed app's view of those secrets ---"
dep="$("$BIN/zeroship" deploy "$ZSHIP" --app="$APP_ID" \
        --control="http://localhost:$ZEROSHIP_CONTROL_PORT" --token="$ADMIN_TOKEN" 2>&1)"
echo "$dep" | grep -q "deploy_hash" && pass "deployed env-probe" \
  || { fail "deploy failed: $dep"; echo "  results: $PASS passed, $FAIL failed"; exit 1; }

probe() {
  curl -s -o "$WORK/deployed.json" -w '%{http_code}' -m 20 -X POST \
    -H 'content-type: application/json' -H "Host: $HOST" \
    "http://localhost:$ZEROSHIP_GATEWAY_PORT/__zeroship/v1/envp.report" -d '{"json":{}}'
}
code=""
for _ in $(seq 1 25); do
  code="$(probe)"; [ "$code" = "200" ] && break
  sleep 2
done
if [ "$code" = "200" ]; then
  pass "gateway routes to the deployed app (envp.report -> 200)"
else
  fail "gateway never routed to the app (last code=$code) -- nothing below can be judged"
  tail -20 "$WORK/gate.log"
  echo ""; echo "  secret expose CLI: $PASS passed, $FAIL failed"
  exit 1
fi

# Read one field ($1 = a JS expression over the unwrapped result object `r`)
# or one key-membership out of the probe response.
jread() {
  node -e '
const fs=require("fs");
let j; try { j=JSON.parse(fs.readFileSync(process.argv[1],"utf8")); } catch { process.stdout.write("<UNPARSEABLE>"); process.exit(0); }
const r=j.json??j.result??j;
let v; try { v=eval(process.argv[2]); } catch(e) { v="<ERR:"+e.message+">"; }
process.stdout.write(v===undefined?"<undefined>":v===null?"<null>":String(v));
' "$WORK/deployed.json" "$1"
}
has_key() {   # $1 surface, $2 key -> yes|no|err
  node -e '
const fs=require("fs");
let j; try { j=JSON.parse(fs.readFileSync(process.argv[1],"utf8")); } catch { process.stdout.write("err"); process.exit(0); }
const r=j.json??j.result??j;
const k=((r[process.argv[2]]||{}).keys)||[];
process.stdout.write(k.includes(process.argv[3])?"yes":"no");
' "$WORK/deployed.json" "$1" "$2"
}

# --- controls, read before believing any verdict ---
fm="$(jread 'r.fixtureMarker')"
[ "$fm" = "$FIXTURE_MARKER" ] \
  && pass "TRANSPORT CONTROL: the response carries '$FIXTURE_MARKER' -- the procedure body made the round trip" \
  || fail "TRANSPORT CONTROL: fixtureMarker was '$fm', expected '$FIXTURE_MARKER'"

pe="$(jread 'r.processEnv.count')"; pi="$(jread 'r.processEnvIndirect.count')"
[ "$pe" = "$pi" ] \
  && pass "INSTRUMENT: both spellings of process.env see the same key count ($pe) -- the build did not fold the probe" \
  || fail "INSTRUMENT: bare process.env sees $pe keys, the computed-key lookup sees $pi -- the BUILD rewrote one; process.env verdicts would be about the bundler, not the platform"

ctl="$(jread 'r.processEnv.control')"
[ "$ctl" = "$VAL_CONTROL" ] \
  && pass "POSITIVE CONTROL: the plaintext var $VAR_CONTROL IS readable in the deployed process.env" \
  || fail "POSITIVE CONTROL: process.env.$VAR_CONTROL was '$ctl', expected '$VAL_CONTROL' -- this probe cannot read process.env at all, so 'absent' below means nothing"

# --- the exposed half: BOTH names, which is the union at runtime ---
for k in "$SEC_A" "$SEC_B"; do
  if [ "$(has_key processEnv "$k")" = "yes" ]; then
    pass "EXPOSED: the deployed process.env carries $k"
  else
    fail "EXPOSED: the deployed process.env does NOT carry $k, though the CLI put it on the expose list. If only one of $SEC_A/$SEC_B is missing, the read-modify-write dropped it."
  fi
done

# --- the withheld half, judged only because the half above passed ---
for k in "$SEC_C" "$SEC_D"; do
  if [ "$(has_key processEnv "$k")" = "yes" ]; then
    fail "OPT-IN BREACH: the deployed process.env carries $k, which is not on the expose list. This is the Node-compat surface, so every npm dependency in the bundle can read it."
  else
    pass "OPT-IN GATE HOLDS: the deployed process.env withholds $k (stored, not exposed)"
  fi
done

# --- the delivery control: the withheld secrets DID reach the isolate ---
# `env` carries every stored secret regardless of the expose list
# (pinned by secret_visible_via_zeroship_env in crates/runtime/tests/call_fetch_handler.rs).
# So a name present here and absent from process.env is the gate working; a
# name absent from BOTH would mean it never arrived, and the verdict above
# would be measuring the wrong thing.
for k in "$SEC_A" "$SEC_B" "$SEC_C" "$SEC_D"; do
  if [ "$(has_key appEnv "$k")" = "yes" ]; then
    pass "DELIVERY CONTROL: $k IS readable on the zeroship \`env\` surface (so its process.env status above is about the expose list, not about arrival)"
  else
    fail "DELIVERY CONTROL: $k is missing from the zeroship \`env\` surface too. The zeroship env is specified to carry every stored secret, so either the merge broke or this secret never reached the isolate."
  fi
done

echo ""
echo "  deployed process.env keys: $(jread 'r.processEnv.keys.join(" ")')"
echo "  deployed env keys        : $(jread 'r.appEnv.keys.join(" ")')"
echo ""
echo "  secret expose CLI: $PASS passed, $FAIL failed"
echo "  MUTATION: crates/zeroship-cli/src/secrets.rs merge_expose Add arm -> vec![key.to_string()]"
echo "            takes this to 27 passed / 8 failed, controls all still green"
[ "$FAIL" -eq 0 ]
