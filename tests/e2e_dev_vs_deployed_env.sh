#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Environment surface: dev vs deployed -- run ONE identical RPC procedure
# against `pnpm dev` and against the same app deployed behind the gateway, then
# (a) assert an ABSOLUTE property of the DEPLOYED response and (b) report the
# dev response beside it.
#
# THE QUESTION THIS ANSWERS: can a creator's app code read a variable that
# exists in the SHELL THAT LAUNCHED THE SERVER but was never deployed to the
# app? Answered separately for each tier and for each of the three surfaces app
# code can reach (`process.env`, `globalThis.__env__`, and the documented
# `env` export of the `zeroship` module).
#
# WHY (a) IS THE POINT, AND WHY A DIFF ALONE CANNOT ANSWER IT.
#   A dev-vs-deployed diff reports DISAGREEMENT. It is structurally incapable of
#   reporting a defect the two tiers SHARE -- and the shape of this particular
#   question makes that failure mode near-certain, because the two tiers' key
#   lists can never match anyway (dev's host env is this developer's machine,
#   deployed's is a CI/server box), so the diff would be red for reasons that
#   are not the finding and would stay red whether or not the canary leaked.
#   `tests/e2e_dev_vs_deployed_errors.sh` documents the same trap from the other
#   direction: its `scrub` collapsed both tiers' stacks identically and the diff
#   went GREEN while production was leaking stack traces to anonymous callers.
#   So this harness does NOT diff key lists. It asserts, absolutely, on the
#   deployed side; it PRINTS the dev side.
#
# THE CANARY. `$CANARY_KEY=$CANARY_VAL` is exported into the shell that launches
#   BOTH tiers and is NEVER deployed as an app var. The value is freshly random
#   per run, so a value baked into a fixture or left in a stale process can
#   never satisfy the assertion by accident.
#
# THE PRECONDITION THE DEPLOYED VERDICT RESTS ON. "The canary is absent from the
#   deployed response" is worth nothing unless the canary was actually IN the
#   deployed worker's environment to be leaked. That is not assumed: it is read
#   off `/proc/<worker-pid>/environ` of the live process, and the verdict is
#   declared UNSAFE if that read does not find it. Without this guard the whole
#   harness passes trivially on any machine where the export failed.
#
# THE POSITIVE CONTROL, differing in ONE variable. `$CONTROL_KEY` IS legitimately
#   delivered to the app -- as a control-plane app var on the deployed tier, via
#   the `ZS_VAR_` prefix on the dev tier -- and MUST be readable in the very same
#   response that reports the canary absent. It differs from the canary in
#   exactly one respect: whether it was deployed. Without it, "canary absent" is
#   equally consistent with the procedure being broken, the env being empty, or
#   the request never having run. A second, independent transport control
#   (`fixtureMarker`, a literal compiled into the bundle) proves the procedure
#   body itself made the round trip.
#
# CAN THIS ASSERTION FAIL? Two mutations, each changing ONE thing.
#   MUTATE=deploy-canary       ALSO set $CANARY_KEY as a control-plane app var.
#                              Same name, same probe, same everything -- only its
#                              deployment status changes. The deployed leak
#                              assertion MUST go RED. This is what proves the
#                              assertion reads the response rather than being
#                              hardcoded to pass.
#   MUTATE=no-worker-canary    Launch the deployed stack WITHOUT the canary in
#                              its environment. The /proc precondition MUST go
#                              RED and the verdict must be declared unsafe. This
#                              is what proves the precondition guard is live.
#
# Prereqs (docs/runbooks/local-dev.md):
#   pnpm build
#   cargo build --release -p zeroship-control -p zeroship-worker -p zeroship-gateway -p zeroship --bins
#   cargo build --release -p zeroship-migrate-adapter --features platform-cli --bin zeroship-platform-migrate
#   docker (this script starts its OWN ephemeral Postgres -- nothing to pre-start)
#   pnpm install --filter ./examples/env-probe...
#
#   ./tests/e2e_dev_vs_deployed_env.sh
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release"
APP="$ROOT/examples/env-probe"
ZSHIP="$APP/dist/app.zship"

# A port band of its own: auth 9398/8398/8308/5458, errors 9399/8399/8309/5459,
# kv 9392/8392/8302/3011, storage 9396/8396/8306/3081, stream 9394/8394/8304/3061.
export CONTROL_PORT="${CONTROL_PORT:-9397}"
export WORKER_PORT="${WORKER_PORT:-8397}"
export GATE_PORT="${GATE_PORT:-8307}"
export PG_PORT="${PG_PORT:-5457}"
export PG_CONTAINER="${PG_CONTAINER:-zs-devdeploy-env-pg}"
DEV_PORT="${DEV_PORT:-3097}"   # examples/env-probe ENV_PROBE_API_PORT default
# VITE's own port. DEV_PORT above is the RUNTIME port. vite was silently taking
# its :5173 global default, which nothing here declared, tracked or freed, so a
# second harness on this machine fought it for the port and the cleanup trap
# could never reclaim it. --strictPort at the call so a conflict fails loudly
# rather than moving to a port nobody watches. Checked against the runtime's
# bad-ports list before choosing it (see #272 and the stream harness). See #272.
VITE_PORT="${VITE_PORT:-5097}"
APP_SLUG="env-probe-dd"
HOST="$APP_SLUG.localhost"
MUTATE="${MUTATE:-none}"

# Must match examples/env-probe/src/index.ts. Re-asserted against that file
# below so the pair cannot drift silently -- a drifted name would make every
# leak assertion trivially green.
# The opt-in secret layer. Secrets are NOT surfaced by being set: only names
# placed in the expose list via `PUT /api/apps/:id/env/expose` reach the
# isolate (crates/control/src/env_handlers.rs:265). Two secrets differing in
# exactly that one respect turn "a secret did not arrive" from an assumption
# into a measurement: EXPOSED must appear, HIDDEN must not. Before this, no
# harness had populated the layer at all, so nothing was known either way.
SECRET_EXPOSED_KEY="ZS_SECRET_EXPOSED"
SECRET_HIDDEN_KEY="ZS_SECRET_HIDDEN"
CANARY_KEY="ZS_LEAK_PROBE"
CONTROL_KEY="ZS_ENV_CONTROL"
FIXTURE_MARKER="ZSENVP-3c9d-fixture"

# Fresh per run. A canary whose value is fixed could be satisfied by a stale
# server, a leftover export, or a value compiled into the fixture; a fresh one
# can only be satisfied by THIS run's environment.
CANARY_VAL="zs-leak-canary-$$-$(date +%s)-$RANDOM"
CONTROL_VAL="zs-deployed-control-$$-$RANDOM"

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
  "crates/runtime/src crates/worker/src crates/gateway/src crates/control/src crates/core/src crates/cli/src sdks/bootstrap/src sdks/rpc/src sdks/vite-plugin/src" \
  "zeroship zeroship-worker zeroship-gate zeroship-control" \
  || { _zs_fresh_rc=$?; [ "$_zs_fresh_rc" -ne 0 ] && exit "$_zs_fresh_rc"; }

echo "=== environment surface: dev vs deployed (env-probe) ==="
echo "  mutation: $MUTATE"
echo "  canary:   $CANARY_KEY=$CANARY_VAL   (NEVER deployed as an app var)"
echo "  control:  $CONTROL_KEY=$CONTROL_VAL (deployed as an app var / ZS_VAR_ on dev)"

# --- 0b. the canary must not already be in this shell -----------------------
# If a previous run (or the operator) left $CANARY_KEY exported with a DIFFERENT
# value, the fresh value below still wins, but a pre-existing $CONTROL_KEY could
# reach the runtime through a path this harness does not control and make the
# positive control pass for the wrong reason. Refuse rather than report on it.
if [ -n "${!CONTROL_KEY:-}" ]; then
  fail "$CONTROL_KEY is already set in this shell ('${!CONTROL_KEY}') -- it would reach the runtime through an uncontrolled path; unset it and re-run"
  exit 2
fi
pass "$CONTROL_KEY unset in the harness environment (the positive control can only come from the platform)"

# ---------------------------------------------------------------------------
# The probe. ONE function, both sides. `$1` is the base URL, `$2` the raw file.
# ---------------------------------------------------------------------------
probe() {
  local base="$1" out="$2"
  curl -s -o "$out" -w '%{http_code}' -m 20 -X POST \
    -H 'content-type: application/json' -H "Host: $HOST" \
    "$base/__zeroship/v1/envp.report" -d '{"json":{}}'
}

# Read one field out of a probe response. `$1` file, `$2` a JS expression over
# the unwrapped result object `r`.
jread() {
  node -e '
const fs=require("fs");
let j; try { j=JSON.parse(fs.readFileSync(process.argv[1],"utf8")); } catch { process.stdout.write("<UNPARSEABLE>"); process.exit(0); }
const r=j.json??j.result??j;
let v; try { v=eval(process.argv[2]); } catch(e) { v="<ERR:"+e.message+">"; }
process.stdout.write(v===undefined?"<undefined>":v===null?"<null>":String(v));
' "$1" "$2"
}

# ---------------------------------------------------------------------------
# 1. Build the app, and assert the FIXTURE INVARIANTS the comparison rests on.
# ---------------------------------------------------------------------------
WORK_EARLY="$(mktemp -d -t zs-envprobe-XXXXXX)"
WORK="$WORK_EARLY"   # stack_up replaces this; the build needs a scratch dir now

( cd "$APP" && pnpm build ) > "$WORK/build.log" 2>&1
[ -f "$ZSHIP" ] && pass "built app.zship ($(du -k "$ZSHIP" | cut -f1)KB)" \
  || { fail "build produced no app.zship"; tail -30 "$WORK/build.log"; exit 1; }

# The names in this script MUST equal the ones the fixture reads, or every
# assertion keyed on them is vacuously green.
for pair in "CANARY_KEY:$CANARY_KEY" "CONTROL_KEY:$CONTROL_KEY" "FIXTURE_MARKER:$FIXTURE_MARKER"; do
  name="${pair%%:*}"; val="${pair#*:}"
  grep -qF "\"$val\"" "$APP/src/index.ts" \
    && pass "$name '$val' matches examples/env-probe/src/index.ts" \
    || { fail "drift: '$val' is not in $APP/src/index.ts"; exit 1; }
done

# The procedure must be anon in the BUILT manifest. A gated procedure is answered
# by the GATEWAY before dispatch, so the deployed response would describe the
# gateway's refusal rather than the worker's environment -- and that reads as a
# clean "no leak".
d="$WORK/unpack"; mkdir -p "$d"; tar -xf "$ZSHIP" -C "$d"
grep -qE '"rpc:envp\.report":\{[^}]*"auth":"anon"' "$d/manifest.json" \
  && pass "envp.report is anon in the manifest (deployed calls reach the WORKER)" \
  || { fail "envp.report is not anon in the manifest -- deployed calls never reach the worker"; exit 1; }

# ---------------------------------------------------------------------------
# 2. Dev side. `pnpm dev` -> vite -> `zeroship serve` child. The canary goes in
#    the shell that launches vite, exactly as a developer's would.
# ---------------------------------------------------------------------------
for _p in "$DEV_PORT" "$VITE_PORT"; do
  lsof -ti :"$_p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
done
(
  cd "$APP" && env "$CANARY_KEY=$CANARY_VAL" "ZS_VAR_$CONTROL_KEY=$CONTROL_VAL" \
    ./node_modules/.bin/vite --port "$VITE_PORT" --strictPort > "$WORK/dev.log" 2>&1
) & PIDS+=($!)
for _ in $(seq 1 25); do
  [ "$(probe "http://localhost:$DEV_PORT" "$WORK/dev.json")" = "200" ] && break
  sleep 2
done
DEV_CODE="$(probe "http://localhost:$DEV_PORT" "$WORK/dev.json")"
[ "$DEV_CODE" = "200" ] && pass "dev app reachable on :$DEV_PORT (HTTP $DEV_CODE)" \
  || { fail "dev app never came up (HTTP $DEV_CODE)"; tail -30 "$WORK/dev.log"; exit 1; }

# WHICH VECTOR DOES `pnpm dev` ACTUALLY TAKE? Asserted against the live process
# table, not inferred from the plugin source: if the dev tier were NOT the CLI
# `serve` vector, everything this harness reports about dev would describe a
# different code path than the one under examination.
DEV_CHILD="$(pgrep -f "zeroship serve .*--port=$DEV_PORT" | head -1)"
if [ -n "$DEV_CHILD" ] && [ -r "/proc/$DEV_CHILD/cmdline" ]; then
  pass "dev tier IS the CLI serve vector: $(tr '\0' ' ' < "/proc/$DEV_CHILD/cmdline")"
  # NOT `tr ... | grep -q`, and the reason is a LATENT size-dependence rather
  # than an observed failure -- stated that way round because the numbers below
  # do not support the stronger claim this comment used to make.
  #
  # THE HAZARD. `grep -q` exits on the FIRST match. If `tr` is still writing at
  # that moment it takes SIGPIPE (141), and `set -o pipefail` promotes 141 to the
  # pipeline's status -- so the `if` takes the else arm EXACTLY WHEN THE CANARY IS
  # PRESENT. But `tr` is only still writing if the data exceeds what the pipe
  # buffer can absorb, so the inversion is CONDITIONAL ON INPUT SIZE.
  #
  # MEASURED 2026-08-10 on this machine, same pipeline, match at the FRONT, only
  # the input size varying:
  #     17 KB -> 0     52 KB -> 0     69 KB -> 0     134 KB -> 141  INVERTED
  #     34 KB -> 0     63 KB -> 0     71 KB -> 0     538 KB -> 141  INVERTED
  # Threshold is between 71 KB and 134 KB, consistent with a 64 KB pipe buffer
  # plus tr's own buffering.
  #
  # AND THE REAL INPUTS ARE UNDER IT. `wc -c < /proc/PID/environ` on live
  # processes of exactly the classes these two sites read: 50605, 50605, 50917,
  # 49455 bytes. So this pipeline does NOT invert here today -- confirmed by
  # running it: reverted to the pipeline form, the assertion below still reported
  # the canary PRESENT. Roughly 1.5x of headroom, and this box has an unusually
  # fat environment, so a leaner one has more.
  #
  # WHY REWRITE IT ANYWAY. A check whose correctness depends on staying under a
  # buffer threshold is a trap that springs later: more secrets, more app vars, a
  # CI image with a fatter env, and 50 KB walks toward 134 KB with nothing
  # failing loudly on the way. Redirecting to a file removes the pipeline and the
  # size-dependence with it, at no cost.
  #
  # NOT ESTABLISHED, and previously asserted here: that this precondition was
  # ever observed red for this reason. That claim cited three consecutive runs; I
  # could not reproduce it and the measurements above do not explain it. If those
  # reds were real they had some other cause, which is still unexplained.
  tr '\0' '\n' < "/proc/$DEV_CHILD/environ" > "$WORK/dev-child.environ" 2>/dev/null || true
  if grep -qF "$CANARY_KEY=$CANARY_VAL" "$WORK/dev-child.environ"; then
    pass "the dev runtime child (pid $DEV_CHILD) HAS the canary in its process environment"
  else
    fail "the dev runtime child (pid $DEV_CHILD) does NOT have the canary -- the dev verdict below is unsafe"
  fi
else
  fail "no 'zeroship serve --port=$DEV_PORT' process found -- cannot confirm which vector pnpm dev takes"
fi

# ---------------------------------------------------------------------------
# 3. Deployed side: real stack, real deploy, real gateway.
# ---------------------------------------------------------------------------
DEV_WORK="$WORK"

# THE CANARY GOES INTO THE SHELL THAT LAUNCHES THE PLATFORM BINARIES. stack_up
# starts control/worker/gateway as children of this shell, so exporting here is
# what puts the canary in the deployed worker's own environment -- the thing the
# absolute assertion is about. MUTATE=no-worker-canary withholds it.
if [ "$MUTATE" = "no-worker-canary" ]; then
  echo "  MUTATED: the deployed stack is launched WITHOUT $CANARY_KEY (the /proc precondition must go RED)"
else
  export "$CANARY_KEY=$CANARY_VAL"
fi

stack_up || { fail "stack bring-up failed"; exit 1; }   # stack_up resets $WORK
cp "$DEV_WORK/dev.json" "$WORK/dev.json"
cp "$DEV_WORK/dev.log"  "$WORK/dev.log" 2>/dev/null || true
mint_admin_pat || exit 1

# Create the app FIRST so the vars land before the worker ever loads a bundle
# (the worker builds its isolate from the env snapshot at load time).
APP_JSON="$(curl -s -X POST "http://localhost:$CONTROL_PORT/api/apps" \
  -H 'Content-Type: application/json' -H "Authorization: Bearer $PAT" \
  -d "{\"name\":\"$APP_SLUG\"}")"
APP_ID="$(printf '%s' "$APP_JSON" | _stk_jget '.id')"
[ -n "$APP_ID" ] && pass "created app $APP_ID" || { fail "create app: $APP_JSON"; exit 1; }

# The POSITIVE CONTROL: a legitimately-deployed app var.
vc="$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://localhost:$CONTROL_PORT/api/apps/$APP_ID/vars" \
  -H 'Content-Type: application/json' -H "Authorization: Bearer $PAT" \
  -d "{\"key\":\"$CONTROL_KEY\",\"value\":\"$CONTROL_VAL\"}")"
[ "$vc" = "204" ] && pass "deployed app var $CONTROL_KEY (HTTP $vc)" \
  || { fail "could not set app var $CONTROL_KEY (HTTP $vc) -- the positive control is unavailable"; }

# THE OPT-IN SECRET LAYER: two secrets, one opted into the expose list and one
# not. Setting a secret is not the same as exposing it.
for k in "$SECRET_EXPOSED_KEY" "$SECRET_HIDDEN_KEY"; do
  sc="$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://localhost:$CONTROL_PORT/api/apps/$APP_ID/secrets" \
    -H 'Content-Type: application/json' -H "Authorization: Bearer $PAT" \
    -d "{\"key\":\"$k\",\"value\":\"secret-value-for-$k\"}")"
  [ "$sc" = "204" ] && pass "stored secret $k (HTTP $sc)" \
    || fail "could not store secret $k (HTTP $sc) -- the secret-layer arms below are vacuous"
done
xc="$(curl -s -o /dev/null -w '%{http_code}' -X PUT "http://localhost:$CONTROL_PORT/api/apps/$APP_ID/env/expose" \
  -H 'Content-Type: application/json' -H "Authorization: Bearer $PAT" \
  -d "{\"keys\":[\"$SECRET_EXPOSED_KEY\"]}")"
[ "$xc" = "200" ] || [ "$xc" = "204" ] \
  && pass "opted $SECRET_EXPOSED_KEY into the expose list, left $SECRET_HIDDEN_KEY out (HTTP $xc)" \
  || fail "could not set the expose list (HTTP $xc) -- cannot tell an unexposed secret from a broken expose call"

if [ "$MUTATE" = "shadow-app-id" ]; then
  # THE LAYERING QUESTION, not a red-before-green. `crates/worker/src/cache.rs`
  # injects `APP_ID` as a worker-internal var; `crates/runtime/src/core/init.rs`
  # layers creator vars OVER it. So a creator var literally named `APP_ID` should
  # win. Deploying one and reading it back is the only way to know whether the
  # documented precedence is the real precedence.
  sc="$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://localhost:$CONTROL_PORT/api/apps/$APP_ID/vars" \
    -H 'Content-Type: application/json' -H "Authorization: Bearer $PAT" \
    -d "{\"key\":\"APP_ID\",\"value\":\"zs-shadowed-app-id\"}")"
  echo "  MUTATED: a creator var named APP_ID is deployed (HTTP $sc) -- reported below, not asserted"
fi

if [ "$MUTATE" = "deploy-canary" ]; then
  # RED-BEFORE-GREEN for the ABSOLUTE half. ONE variable: the canary's
  # deployment status. Same name, same value, same probe, same assertion.
  mc="$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://localhost:$CONTROL_PORT/api/apps/$APP_ID/vars" \
    -H 'Content-Type: application/json' -H "Authorization: Bearer $PAT" \
    -d "{\"key\":\"$CANARY_KEY\",\"value\":\"$CANARY_VAL\"}")"
  echo "  MUTATED: $CANARY_KEY is ALSO deployed as an app var (HTTP $mc) -- the leak assertions must go RED"
fi

dep="$("$BIN/zeroship" deploy "$ZSHIP" --app="$APP_ID" --control="http://localhost:$CONTROL_PORT" --token="$PAT" 2>&1)"
echo "$dep" | grep -q "deploy_hash" && pass "deployed env-probe" \
  || { fail "deploy failed: $dep"; exit 1; }

# --- THE PRECONDITION. Read the LIVE worker process, do not assume the export.
worker_pid="$(sed -n '2p' "$PIDFILE")"
CANARY_IN_WORKER=0
if [ -n "$worker_pid" ] && [ -r "/proc/$worker_pid/environ" ]; then
  # See the note on the dev-child check above: `tr ... | grep -q` under
  # `set -o pipefail` reports FAILURE precisely when it matches, because grep -q
  # exits early and tr dies of SIGPIPE. That made this precondition -- the one
  # that decides whether every leak verdict below means anything -- unable to
  # report success at all. No pipeline here.
  tr '\0' '\n' < "/proc/$worker_pid/environ" > "$WORK/worker.environ" 2>/dev/null || true
  if grep -qF "$CANARY_KEY=$CANARY_VAL" "$WORK/worker.environ"; then
    CANARY_IN_WORKER=1
    pass "PRECONDITION: the deployed worker (pid $worker_pid) HAS $CANARY_KEY=$CANARY_VAL in its own process environment -- there is something to leak"
  else
    fail "PRECONDITION: the deployed worker (pid $worker_pid) does NOT have $CANARY_KEY in its environment -- every 'no leak' verdict below is VACUOUS"
  fi
else
  fail "PRECONDITION: could not read /proc/$worker_pid/environ -- cannot establish that the canary was available to leak"
fi

# Wait for the gateway to pull the route (an unrouted call is a 404/503).
ready=0
for _ in $(seq 1 25); do
  c="$(probe "http://localhost:$GATE_PORT" "$WORK/deployed.json")"
  [ "$c" = "200" ] && { ready=1; break; }
  sleep 2
done
[ "$ready" = "1" ] && pass "gateway routes to the deployed app (envp.report -> 200)" \
  || { fail "gateway never routed to the app (last code=$c)"; tail -20 "$WORK/gate.log"; }

# ---------------------------------------------------------------------------
# 4a. THE CONTROLS. Read these before believing any verdict in 4b.
# ---------------------------------------------------------------------------
echo ""
echo "--- controls (deployed) ---"
CONTROLS_OK=1

fm="$(jread "$WORK/deployed.json" 'r.fixtureMarker')"
if [ "$fm" = "$FIXTURE_MARKER" ]; then
  pass "TRANSPORT CONTROL: the deployed response carries '$FIXTURE_MARKER' -- the procedure body made the round trip"
else
  fail "TRANSPORT CONTROL: fixtureMarker was '$fm', expected '$FIXTURE_MARKER'"; CONTROLS_OK=0
fi

for s in appEnv processEnv processEnvIndirect globalEnv; do
  got="$(jread "$WORK/deployed.json" "r.$s.control")"
  if [ "$got" = "$CONTROL_VAL" ]; then
    pass "POSITIVE CONTROL: deployed $s.$CONTROL_KEY == '$CONTROL_VAL' (a legitimately-deployed var IS visible here)"
  else
    fail "POSITIVE CONTROL: deployed $s.$CONTROL_KEY was '$got', expected '$CONTROL_VAL' -- 'canary absent' on this surface is NOT a safety result; it is indistinguishable from a probe that cannot read the surface at all"
    CONTROLS_OK=0
  fi
done

# THE OPT-IN SECRET LAYER, measured rather than assumed. Both secrets were
# stored; only one was opted into the expose list.
#
# THE EXPECTATION DIFFERS BY SURFACE, and reading one rule onto all four is a
# mistake this loop used to make -- it failed `appEnv` for behaviour the
# platform deliberately has:
#
#   process.env / __env__  vars + ONLY the secrets named in `expose`
#   zeroship `env`         vars + ALL secrets; `expose` does not apply
#
# The split is a blast-radius control, not an app boundary. `process.env` is
# the Node-compat surface that any npm dependency reads without the creator
# writing a line, so a secret reaches it only on request. The `zeroship` env
# is the audited surface the creator's own code names explicitly, and an app
# reading its own secrets there is the point of storing them. Pinned by
# `secret_visible_via_zeroship_env` and `secret_does_not_appear_in_process_env`
# in crates/runtime/tests/call_fetch_handler.rs.
#
# So both directions are load-bearing here: a hidden secret appearing in
# process.env means the opt-in is not a gate, and a hidden secret MISSING from
# `env` means the documented merge broke.
for s in appEnv processEnv processEnvIndirect globalEnv; do
  has_exposed="$(node -e 'const r=require(process.argv[1]);const k=(((r.json||r)[process.argv[2]])||{}).keys||[];process.stdout.write(k.includes(process.argv[3])?"yes":"no")' "$WORK/deployed.json" "$s" "$SECRET_EXPOSED_KEY" 2>/dev/null || echo "err")"
  has_hidden="$(node -e 'const r=require(process.argv[1]);const k=(((r.json||r)[process.argv[2]])||{}).keys||[];process.stdout.write(k.includes(process.argv[3])?"yes":"no")' "$WORK/deployed.json" "$s" "$SECRET_HIDDEN_KEY" 2>/dev/null || echo "err")"
  echo "    $s: exposed-secret-present=$has_exposed  hidden-secret-present=$has_hidden"
  if [ "$s" = "appEnv" ]; then
    if [ "$has_hidden" = "yes" ]; then
      pass "MERGE INTACT: deployed $s carries $SECRET_HIDDEN_KEY without an expose entry, which is the documented zeroship-env contract"
    else
      fail "MERGE BROKEN: deployed $s omits $SECRET_HIDDEN_KEY. The zeroship env is specified to carry every stored secret regardless of the expose list, so an app can no longer read a secret it stored."
    fi
  elif [ "$has_hidden" = "yes" ]; then
    fail "OPT-IN BREACH: deployed $s exposes $SECRET_HIDDEN_KEY, which was never added to the expose list. This is the Node-compat surface, so the secret is now readable by every npm dependency in the bundle."
  else
    pass "OPT-IN GATE HOLDS: deployed $s withholds $SECRET_HIDDEN_KEY (present in the store, absent from the expose list)"
  fi
done
EXPOSED_ANY="$(node -e 'const r=require(process.argv[1]);const o=(r.json||r);const k=process.argv[2];process.stdout.write(["appEnv","processEnv","processEnvIndirect","globalEnv"].some(s=>(((o[s])||{}).keys||[]).includes(k))?"yes":"no")' "$WORK/deployed.json" "$SECRET_EXPOSED_KEY" 2>/dev/null || echo "err")"
if [ "$EXPOSED_ANY" = "yes" ]; then
  pass "EXPOSED HALF DELIVERS: $SECRET_EXPOSED_KEY reached the deployed app on at least one surface (this says nothing about the hidden one -- the per-surface loop above is what judges the gate)"
else
  fail "OPT-IN SECRET LAYER DOES NOT DELIVER: $SECRET_EXPOSED_KEY was stored AND opted into the expose list, yet reached no deployed surface. The hidden secret is correctly absent, so this is not the opt-in gate working -- it is the exposed half not arriving. docs/pilot/e2e-scenarios.md describes this as layer 2 of the deployed env; on this evidence that description is aspirational."
fi

# WHY A SURFACE CAN READ EMPTY. `crates/runtime/src/core/init.rs` sets
# `process.env` and `globalThis.__env__` to THE SAME V8 object, and that is the
# only write to `__env__` in the runtime -- so the two MUST agree. If they do
# not, `globalThis.process` is no longer the object the runtime installed, and
# an empty `process.env` is a REPLACED global rather than a withheld one. Those
# are different findings and the control failure above cannot tell them apart on
# its own.
echo ""
echo "--- identity of the \`process\` global ---"
for tier in deployed dev; do
  same="$(jread "$WORK/$tier.json" 'r.runtimeShape.processEnvIsGlobalEnv')"
  echo "  $tier: typeof process=$(jread "$WORK/$tier.json" 'r.runtimeShape.processType')" \
       "typeof process.env=$(jread "$WORK/$tier.json" 'r.runtimeShape.processEnvType')" \
       "process.env===__env__: $same"
  echo "      own keys of process: $(jread "$WORK/$tier.json" 'r.runtimeShape.processOwnKeys.join(" ")')"
done
if [ "$(jread "$WORK/deployed.json" 'r.runtimeShape.processEnvIsGlobalEnv')" != "true" ]; then
  fail "the deployed \`process\` global is NOT the one the runtime installed (process.env !== __env__) -- Node-compat env reads are answered by something else in a deployed app"
fi

# THE INSTRUMENT CHECK, and the reason this harness has four surfaces instead of
# three. `processEnv` and `processEnvIndirect` read THE SAME OBJECT and differ
# only in SPELLING: a bare `process.env` chain versus a computed-key lookup no
# bundler can match statically. The production `.zship` build runs
# `ssr.target: "webworker"`, which statically rewrites `process.env` to `{}`;
# `sdks/vite-plugin/src/build.ts` carries `define: { "process.env": "process.env" }`
# to defeat exactly that. If the two spellings disagree, the difference was
# introduced by the BUILD, and any conclusion drawn from the bare reading alone
# is about the bundler, not about the platform.
#
# This is not hypothetical: an earlier revision of the fixture read
# `globalThis.process?.env`, which the define does NOT cover. It compiled to a
# literal `{}`, and the deployed tier duly reported an empty `process.env` while
# `__env__` -- the same V8 object -- reported three keys. Read without this
# check, that is indistinguishable from "the platform withholds process.env in
# production", which is a security conclusion drawn from a compiler artefact.
for tier in deployed dev; do
  a="$(jread "$WORK/$tier.json" 'r.processEnv.count')"
  b="$(jread "$WORK/$tier.json" 'r.processEnvIndirect.count')"
  if [ "$a" = "$b" ]; then
    pass "INSTRUMENT: $tier reads the same key count through both spellings of process.env ($a) -- the build did not fold the probe"
  else
    fail "INSTRUMENT: $tier disagrees with itself about process.env: bare chain sees $a keys, computed-key lookup sees $b -- the BUILD rewrote one of them; deployed process.env verdicts are about the bundler, not the platform"
    CONTROLS_OK=0
  fi
done

# ---------------------------------------------------------------------------
# 4b. THE ABSOLUTE QUESTION: can the DEPLOYED app read the canary?
#     Asserted independently of anything the dev tier does.
# ---------------------------------------------------------------------------
echo ""
echo "--- can the DEPLOYED app read a variable from the launching shell? ---"
leaks=0
for s in processEnv processEnvIndirect globalEnv appEnv; do
  got="$(jread "$WORK/deployed.json" "r.$s.canary")"
  cnt="$(jread "$WORK/deployed.json" "r.$s.count")"
  if [ "$got" = "<null>" ]; then
    pass "deployed $s: $CANARY_KEY is ABSENT ($cnt keys total)"
  else
    leaks=$((leaks+1))
    fail "LEAK: deployed $s.$CANARY_KEY == '$got' -- the launching shell's environment reached app code"
  fi
done

# The named host vars are a separate, sharper question: DATABASE_URL is on the
# worker's own command line, so its presence would hand an app the platform's
# database credentials.
echo ""
echo "--- named host variables, deployed ---"
for s in processEnv processEnvIndirect globalEnv appEnv; do
  for n in DATABASE_URL HOME PATH PWD; do
    got="$(jread "$WORK/deployed.json" "r.$s.hostVars.$n")"
    if [ "$got" = "<null>" ]; then
      echo "  absent   $s.$n"
    else
      leaks=$((leaks+1))
      fail "LEAK: deployed $s.$n == '$(printf '%s' "$got" | cut -c1-60)'"
    fi
  done
done

# The worker-internal layer, reported so its precedence is visible. Under
# MUTATE=shadow-app-id a creator var of the same name is also deployed, and the
# value below says which layer won.
echo ""
echo "--- worker-internal vars, deployed (reported, not asserted) ---"
for s in processEnv appEnv; do
  for n in APP_ID ZEROSHIP_DEPLOY_ID; do
    echo "  $s.$n = $(jread "$WORK/deployed.json" "r.$s.workerVars.$n")"
  done
done
echo "  (the deployed app id for this run is $APP_ID)"

if [ "$CANARY_IN_WORKER" != "1" ]; then
  echo ""
  echo "  (!) the verdicts above are VACUOUS: the canary was never in the worker's environment"
fi
if [ "$CONTROLS_OK" != "1" ]; then
  echo "  (!) the verdicts above are UNSAFE: a control failed"
fi

# ---------------------------------------------------------------------------
# 5. The DEV tier, reported beside it -- NOT diffed.
#    Key lists cannot match across tiers (different machines), so a diff would
#    be red for reasons that are not the finding.
# ---------------------------------------------------------------------------
echo ""
echo "--- the SAME procedure on the DEV tier ---"
for s in processEnv processEnvIndirect globalEnv appEnv; do
  got="$(jread "$WORK/dev.json" "r.$s.canary")"
  cnt="$(jread "$WORK/dev.json" "r.$s.count")"
  ctl="$(jread "$WORK/dev.json" "r.$s.control")"
  if [ "$got" = "<null>" ]; then
    echo "  dev $s: canary ABSENT   ($cnt keys, control=$ctl)"
  else
    echo "  dev $s: canary READABLE ('$got') ($cnt keys, control=$ctl)"
  fi
done
echo "  dev named host vars:"
for s in processEnv processEnvIndirect globalEnv appEnv; do
  for n in DATABASE_URL HOME; do
    echo "    $s.$n = $(jread "$WORK/dev.json" "r.$s.hostVars.$n" | cut -c1-60)"
  done
done

echo ""
echo "  --- deployed response (verbatim) ---"
sed 's/^/  /' "$WORK/deployed.json"; echo ""
echo "  --- dev response (verbatim, key lists truncated to 400 chars each) ---"
node -e '
const fs=require("fs");
const j=JSON.parse(fs.readFileSync(process.argv[1],"utf8"));
const r=j.json??j.result??j;
for (const s of ["processEnv","globalEnv","appEnv"]) {
  if (!r[s]) continue;
  const k=(r[s].keys||[]).join(" ");
  r[s].keys = k.length>400 ? k.slice(0,400)+" ...(+"+((r[s].keys||[]).length)+" keys total)" : r[s].keys;
}
console.log(JSON.stringify(r,null,1).split("\n").map(l=>"  "+l).join("\n"));
' "$WORK/dev.json"

# --- The floor: a MEASURED minimum, and the guard against a green run over ---
#     nothing. This script exits on $FAIL alone, and $FAIL is 0 both when every
#     assertion passed and when NO assertion ran. That matters more here than in
#     the sibling harnesses, because almost every verdict in this file is an
#     ABSENCE ("the canary is ABSENT from this surface") and an absence is what
#     an empty response also looks like. The preconditions and the positive
#     control exist to separate those two; the floor is what notices when the
#     preconditions themselves stop running. This repo has shipped three gates
#     that passed over zero tests (#102/#103/#112).
#
# THE FLOOR IS A MEASUREMENT. Taken 2026-08-10 on this tree, running this script
# unmodified:
#
#     env dev vs deployed: 40 passed, 0 failed, 0 leaks        (exit 0)
#
# (38/2 before the same day's fix to the two /proc preconditions, which could
# never report success -- see the notes at those two call sites.)
#
# CROSS-CHECKED against a second, independent instrument: CALL SITES in the
# source, with every loop multiplied out.
#    1  CONTROL_KEY unset in this shell
#    1  built app.zship
#    3  the `for pair in CANARY_KEY CONTROL_KEY FIXTURE_MARKER` fixture-agreement loop
#    1  envp.report is anon in the manifest
#    1  dev app reachable
#    1  dev tier IS the CLI serve vector
#    1  the dev child HAS the canary
#    7  tests/lib/e2e_stack.sh `_stk_ok` sites (PG, init.sql, migrations,
#       control, worker, gateway, pat+jwt) -- these live in a SHARED library and
#       are the reason a naive `grep -c 'pass "' ` on this file alone under-counts
#    1  created app
#    1  deployed app var
#    2  the `for k in EXPOSED HIDDEN` stored-secret loop
#    1  opted into the expose list
#    1  deployed env-probe
#    1  the /proc PRECONDITION
#    1  gateway routes to the deployed app
#    1  TRANSPORT CONTROL
#    4  POSITIVE CONTROL, once per surface
#    4  the secret opt-in gate, once per surface (appEnv takes the MERGE INTACT
#       arm, the other three take OPT-IN GATE HOLDS -- different arms, always
#       exactly four outcomes)
#    1  EXPOSED HALF DELIVERS
#    2  INSTRUMENT, once per tier
#    4  the canary-ABSENT verdict, once per surface
#   = 40. Dynamic and static agree, and they fail differently: the dynamic count
#   moves when a tier stops answering, the static one when an assertion leaves
#   the source.
#
# NO HEADROOM: the total is fixed by the source, not discovered at run time.
#
# WHAT THE FLOOR DOES NOT CATCH: substitution. Swapping one assertion for an
# easier one keeps the total at 40. Nothing here can see that; review can.
ENV_MIN_PASSED="${ENV_MIN_PASSED:-40}"

echo ""
echo "  env dev vs deployed: $PASS passed, $FAIL failed, $leaks deployed surfaces leaking a host variable  (floor $ENV_MIN_PASSED)"

# A MUTATION run is EXPECTED to fail (MUTATE=no-worker-canary must turn the
# precondition red, MUTATE=deploy-canary must turn the leak verdicts red), so
# the floor is what still has to hold there.
rc=0
[ "$FAIL" -eq 0 ] || rc=1
if [ "$PASS" -lt "$ENV_MIN_PASSED" ]; then
  echo "FAIL: only $PASS assertions passed, fewer than the $ENV_MIN_PASSED this gate expects." >&2
  echo "      Assertions do not vanish by accident. Nearly every verdict here is an" >&2
  echo "      ABSENCE, and an absence reads the same as a response that never came," >&2
  echo "      so a shrinking count is the signal that the controls stopped running." >&2
  echo "      If an assertion was removed deliberately, lower ENV_MIN_PASSED in the" >&2
  echo "      same change and say why; do not treat the gap as slack." >&2
  rc=1
fi
exit "$rc"
