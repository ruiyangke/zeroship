#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Golden path — the "build locally → deploy → run" chain, end-to-end, with a
# REAL vite-built app (examples/starter), not a hand-packed fixture.
#
# This is the canonical creator flow under the build-local strategy: an AI
# coding agent (Claude Code / Codex) builds a zeroship app on the creator's
# machine, `pnpm build` produces dist/app.zship, and `zeroship deploy` ships
# it to the platform — then the gateway serves it.
#
# Proves: vite-plugin build → .zship → control-plane deploy → worker load →
# gateway serve (static assets) + (best-effort) an RPC round-trip.
#
# Prereqs (see docs/runbooks/local-dev.md):
#   - service/CLI binaries: cargo build --release -p zeroship-control -p zeroship-worker -p zeroship-gateway -p zeroship --bins
#   - migration binary: cargo build --release -p zeroship-migrate-adapter --features platform-cli --bin zeroship-platform-migrate
#   - a Postgres reachable at $DATABASE_URL (default: the compose instance on :5440)
#   - examples/starter deps installed (pnpm install) so `pnpm build` works
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release"

# A MISSING TOOL MUST SAY SO, not fail somewhere in the middle.
#
# This file had no prerequisite check at all, and its failures on a missing tool
# are actively misleading rather than merely unhelpful: `tar --zstd -xOf ...
# 2>/dev/null` (:289 and two more) swallows the error and yields an EMPTY
# manifest, so absent zstd reads as a malformed artifact. The CI job installs
# only lsof, zstd and postgresql-client, so the set below is not hypothetical -
# it is the difference between "install zstd" and an afternoon on a phantom
# bundle defect.
#
# The list is MEASURED, by enumerating the commands this file invokes in
# pipeline-head position, not guessed. Coreutils (grep/awk/sed/cut/xargs/paste)
# are deliberately excluded: checking things that cannot be missing is noise
# that trains readers to skip the check.
#
# jq is NOT here on purpose. Its only use is inside step 3's `if [ -n "$TOKEN" ]`
# branch, which CI never takes, so requiring it up front would refuse to run a
# harness that does not need it. It is checked at that branch instead.
#
# exit 2, matching the convention at :241 and :253 - "could not run" is a
# distinct outcome from "ran and something failed", and must not be counted as
# either a pass or a failure.
gp_missing=""
for _t in docker curl node pnpm lsof tar openssl zstd; do
  command -v "$_t" >/dev/null 2>&1 || gp_missing="$gp_missing $_t"
done
if [ -n "$gp_missing" ]; then
  echo "FAIL: golden_path cannot run - missing required tool(s):$gp_missing" >&2
  echo "      Nothing was asserted. This is not a pass and not a failure." >&2
  exit 2
fi
# Dedicated, freshly-migrated DB per run (isolated from the shared `zeroship`
# db) so the run is self-contained + reproducible and never re-provisions a
# stale app.
# The container the dev compose stack creates for the Postgres on :5440. Override
# PG_CONTAINER when running against a differently-named container.
PG_CONTAINER="${PG_CONTAINER:-compose-postgres-1}"
PG_USER="${PG_USER:-postgres}"
PG_DB="${PG_DB:-zeroship_golden}"
DB_URL="${DATABASE_URL:-postgres://postgres:zeroship@localhost:5440/$PG_DB}"
# Distinct ports so this never clashes with a running dev stack.
CONTROL_PORT="${CONTROL_PORT:-9390}"
WORKER_PORT="${WORKER_PORT:-8390}"
GATE_PORT="${GATE_PORT:-8300}"
# Step 10 (the scaffold leg) needs the migration service and a Redis, because
# the app a creator actually receives uses env.db + env.storage + env.kv. A
# three-service stack could still measure the 401, but it could not tell a
# platform 401 from "this harness never gave the app a database" -- and a
# comparison whose deployed side is crippled by the harness proves nothing.
MIGRATED_PORT="${MIGRATED_PORT:-9490}"
REDIS_PORT="${REDIS_PORT:-6390}"
REDIS_CONTAINER="${REDIS_CONTAINER:-zs-golden-redis}"
CONTROL_KEY="gp-ck"
MASTER_KEY="gp-mk"
# Local dev: run the platform without the production secret set (signing keys,
# worker key, …). NEVER use this outside local dev. This used to cite
# tests/m0_gate.sh as the harness it mirrors; that file was deleted once its
# subject moved to the sibling zeroship-builder repo, so the pattern now stands
# on its own rather than pointing at something gone.
export ZEROSHIP_DEV_INSECURE=1
export WORKER_KEY="${WORKER_KEY:-golden-path-worker-key-0123456789abcdef}"
APP_NAME="starter"
STARTER="$ROOT/examples/starter"
ZSHIP="$STARTER/dist/app.zship"

# Dev-server leg ports. The zeroship dev RUNTIME binds a port of its OWN,
# separate from vite's, and `vite --port N` does not move it -- so a harness
# that only knows the vite port frees half of what it started and leaves a
# runtime holding the other half. Every port this script binds is listed here
# and freed by `cleanup`, runtime ports included.
DEV_PORT="${DEV_PORT:-3091}"          # vite, step 6
DEV_RT_PORT="${DEV_RT_PORT:-3140}"    # dev runtime, step 6 (STARTER_API_PORT)
SUP_RT="${SUP_RT:-3141}"              # dev runtime SHARED by 7a and 7b (the collision)
SUP_V1="${SUP_V1:-3142}"              # vite, 7a
SUP_V2="${SUP_V2:-3143}"              # vite, 7b
DB_V="${DB_V:-3144}"                  # vite, step 9 (db-todos data plane)
DB_RT="${DB_RT:-3145}"                # dev runtime, step 9 (DB_TODOS_API_PORT)
PF_V="${PF_V:-3148}"                  # vite, step 9b (fault classification)
PF_RT="${PF_RT:-3149}"                # dev runtime, step 9b
SC_V="${SC_V:-3146}"                  # vite, step 10 (scaffold)
# 3001 is NOT a choice. The scaffold template's vite.config.ts passes no
# `devServerPort`, so its dev runtime binds the plugin's DEFAULT_DEV_PORT --
# and pinning it here would mean editing the copy, which is the one thing that
# must stay byte-identical to what a creator receives. So the harness moves to
# the template's port rather than moving the template to the harness's, and
# frees 3001 first like every other port it binds.
SC_RT="${SC_RT:-3001}"                # dev runtime, step 10 (template default)
DEV_PORTS="$DEV_PORT $DEV_RT_PORT $SUP_RT $SUP_V1 $SUP_V2 $DB_V $DB_RT $PF_V $PF_RT $SC_V $SC_RT"

PASS=0; FAIL=0; PIDS=()
pass() { PASS=$((PASS+1)); echo "  ✓ $1"; }
# The LABEL is recorded, not just the count. Exit status cannot distinguish a
# new regression from the known red-at-HEAD set, because this harness already
# exits 1 on those: a ninth failure changes 8 to 9 in one summary line and
# nothing else. Keeping identities is what lets the block at the bottom say
# which failures were expected and which are new.
declare -a GP_FAILURES=()
fail() { FAIL=$((FAIL+1)); GP_FAILURES+=("$1"); echo "  ✗ $1"; }

# --- The gate must not be able to pass over zero assertions ----------------
#
# Exit status alone cannot tell "everything passed" from "nothing was checked".
# Every branch below that DETECTS a problem calls fail(), which makes FAIL>0 and
# turns the final verdict red - so failures are covered. What is NOT covered by
# the exit status is an assertion that stops FIRING: it emits no pass and no
# fail, the total silently shrinks, and the run is green about less than it used
# to be.
#
# That is not hypothetical here. Step 4's asset probe is guarded by
# `if [ -n "$ASSET" ]`, and $ASSET is scraped out of index.html - so the day the
# starter's index stops referencing a hashed /assets/*.js, the "client JS asset
# served" proof disappears with no output of any kind. Same shape for a step
# deleted wholesale in a refactor.
#
# Two independent guards, because they catch different things:
#
#   1. A TOTAL-PASSED FLOOR. Catches assertions disappearing anywhere.
#   2. A PER-STEP ran-something check against a DECLARED step list. The floor
#      cannot see one small step vanish - step 3 is 1 outcome in 24, so losing
#      it costs 4 percent and any floor loose enough to survive a normal edit
#      survives that too. The per-step check catches it immediately, and
#      declaring the ids (rather than counting the steps that happened to run)
#      is what makes a DELETED step visible rather than merely absent.
#
# THE FLOOR NUMBER IS MEASURED, NOT GUESSED. See the block at the bottom of this
# script for the run it came from and why it sits where it does.
#
# BASH ONLY: GP_EXPECTED_STEPS relies on word splitting of an unquoted
# expansion, which zsh does not do. Under zsh the list collapses to one bogus
# id and the check reports every step missing - loud, not silently green, which
# is the intended direction for a check that cannot run.
#
# `9b` IS IN THIS LIST BECAUSE IT WAS MISSING FROM IT, and the omission cost
# exactly what guard 2 exists to prevent. Measured 2026-08-11: step 9b produces
# 14 of the harness's 75 outcomes, and with it absent from this list the guard
# iterated only the declared ids, so 9b could go completely silent and print
# nothing. A run with 9b's body suppressed scored `52 passed, 8 failed` and the
# ONLY diagnosis was the floor - "either a check stopped firing or one was
# removed" - which is the wrong explanation for a whole step asserting nothing.
# The paragraph above about the list being load-bearing was already there; it
# argued carefully for a list that was incomplete, which is what made it look
# audited. Sub-step ids are easy to miss precisely because they are not numbers.
GP_EXPECTED_STEPS="1 2 2b 2c 3 4 5 6 7 8 9 9b 10 11 13 14 12"
declare -A GP_STEP_OUTCOMES=()
GP_CUR_STEP=""; GP_STEP_BASE=0
# step <id> <title...>  - prints the banner AND opens an accounting window.
step() {
  gp_close_step
  GP_CUR_STEP="$1"; shift
  GP_STEP_BASE=$((PASS + FAIL))
  echo "=== $GP_CUR_STEP. $* ==="
}
gp_close_step() {
  [ -n "$GP_CUR_STEP" ] || return 0
  GP_STEP_OUTCOMES["$GP_CUR_STEP"]=$((PASS + FAIL - GP_STEP_BASE))
  GP_CUR_STEP=""
}
# `-sTCP:LISTEN` is load-bearing. Plain `lsof -ti :$PORT` matches every socket
# with that port on EITHER end, so freeing the worker's port also killed the
# GATEWAY, which merely held a client connection to it -- and the gateway dying
# turned step 8's "worker unavailable" probe into a connection failure (HTTP
# 000) that looked like a platform verdict.
free_ports() { for p in "$@"; do lsof -ti :"$p" -sTCP:LISTEN 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done; }
# Killing the recorded PIDs is not enough: `( cd x && vite )&` records the
# subshell, and the dev server in turn forks a `zeroship serve` child that is
# not in $PIDS at all. Freeing the ports by listener is what actually
# guarantees the next run of this script starts clean.
cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  sleep 1
  free_ports $DEV_PORTS
  # Step 2b's two extra control instances. Freed here as well as inline: the
  # failure that matters is the abort BETWEEN the launch and the inline kill,
  # which would leave a control holding :9391 and make the next run's step 2b
  # green against a process this script never started.
  free_ports "${CFG_PORT:-9391}" "${CFG_PORT_B:-9392}"
  docker rm -f "$REDIS_CONTAINER" >/dev/null 2>&1 || true
  # Step 10's CONTROL leg writes a policy file into the scaffold copy and must
  # not leave it there: the copy's whole value is being byte-identical to the
  # template, and a leftover config.ts would silently turn the next run's
  # measurement into the control. Removed here too, not only on the happy path,
  # because the failure that matters is the one that aborts mid-step.
  rm -rf "$ROOT/examples/scaffold-app/src/server" 2>/dev/null || true
  # The fault-classification leg (#269) arms a PLATFORM fault by making the
  # generated dir read-only. Restored here as well as inline: if the script
  # aborts between the chmod and its restore, read-only artifacts would make
  # every LATER run of this harness -- and the creator's own `pnpm dev` in that
  # checkout -- fail for a reason that has nothing to do with what they changed.
  [ -n "${PF_GEN_DIR:-}" ] && chmod u+w "$PF_GEN_DIR" \
    "$PF_GEN_DIR"/env.db.ts "$PF_GEN_DIR"/schema.runtime.json 2>/dev/null || true
  wait 2>/dev/null || true
}
trap cleanup EXIT

# --- PREREQUISITE: the binaries this harness starts ------------------------
#
# These were documented at the top of this file and checked NOWHERE. MEASURED
# 2026-08-12: with `target/` removed, the run got as far as step 2 and died on
# a raw shell error --
#     tests/golden_path.sh: line 359:
#       .../target/release/zeroship-platform-migrate: No such file or directory
# preceded by a `platform migrations failed` row, which reads as a PLATFORM
# defect. The build command that fixes it is written twelve lines above, in a
# comment the reader has no reason to be looking at once a step has failed.
#
# ABORTS rather than accumulating a failure: every later row would measure a
# stack that was never started, and 100 misleading rows are worse than one
# honest refusal. Exit 2 marks "prerequisite absent" as distinct from exit 1
# "assertions failed", matching tests/create_demo_invoices.sh and
# tests/supabase_deploy_e2e.sh (`ensure_release_bins`).
#
# NO pass row on success, deliberately: a prerequisite is not an assertion
# about the product, and emitting one would shift GOLDEN_MIN_PASSED for a
# reason unrelated to coverage.
gp_missing_bins=()
for _b in dev-provision zeroship zeroship-control zeroship-gate zeroship-migrated \
         zeroship-platform-migrate zeroship-worker; do
  [ -x "$BIN/$_b" ] || gp_missing_bins+=("$_b")
done
if [ "${#gp_missing_bins[@]}" -gt 0 ]; then
  echo "  ✗ PREREQUISITE MISSING: ${#gp_missing_bins[@]} of 7 release binaries are absent from $BIN" >&2
  for _b in "${gp_missing_bins[@]}"; do echo "      - $_b" >&2; done
  echo "      Nothing below would measure the platform: the services never start." >&2
  echo "      Build them with:" >&2
  echo "        cargo build --release -p zeroship-control -p zeroship-worker \\" >&2
  echo "          -p zeroship-gateway -p zeroship -p zeroship-migrated --bins" >&2
  echo "        cargo build --release -p zeroship-migrate-adapter \\" >&2
  echo "          --features platform-cli --bin zeroship-platform-migrate" >&2
  exit 2
fi

# --- HTTP probes that separate "listening" from "answered 2xx" -------------
#
# `curl -sf` conflates the two: it fails on ANY non-2xx, so an app whose RPC
# returns 503 BECAUSE its runtime is down reads identically to one where vite
# never bound a socket. That points the reader at the wrong process -- and,
# with the dev-runtime supervisor added in step 7, 503 is now the EXPECTED
# answer for a dead runtime, so a `-sf` readiness loop would spin its full
# timeout and then report the one thing that is not true.
# curl PRINTS `000` and ALSO exits non-zero when it cannot connect, so the
# obvious `curl ... || echo 000` emits `000000` -- which is not equal to `000`,
# so a "did anything answer?" check reads a dead port as alive. That produced a
# false PASS ("vite IS serving HTTP (status 000000)") on the first run of this
# step. Substitute only when curl printed nothing at all.
http_status() {
  local code
  code=$(curl -s -o /dev/null -w '%{http_code}' -m 5 "$1" 2>/dev/null)
  echo "${code:-000}"
}
# 0 as soon as anything at all answers (000 == nothing listening / no response).
wait_listening() {
  local url=$1 tries=${2:-40}
  for _ in $(seq 1 "$tries"); do
    [ "$(http_status "$url")" != "000" ] && return 0
    sleep 1
  done
  return 1
}
# 0 only on a 2xx.
wait_http_ok() {
  local url=$1 tries=${2:-40} code
  for _ in $(seq 1 "$tries"); do
    code=$(http_status "$url")
    case "$code" in 2??) return 0 ;; esac
    sleep 1
  done
  return 1
}

echo "============================================"
echo "  zeroship golden path (build-local → deploy)"
echo "============================================"

# Step 1 runs `pnpm build` inside the EXAMPLE, which consumes whatever
# `sdks/vite-plugin/dist/` is already on disk. It never rebuilds the plugin. So
# an edit to the plugin's own source can be silently untested: the run is green
# about a dist that predates the change, and nothing in the output says so.
#
# Every other dev-vs-deployed harness in this directory sources
# `tests/lib/binary_freshness.sh`; this script was the only one with no
# freshness check of any kind. WARNS by default (mtime is a weak proxy for
# provenance -- a rebase touches a file without changing it); set
# ZS_FRESHNESS_STRICT=1 to refuse, which is what CI wants.
# shellcheck source=lib/binary_freshness.sh
source "$ROOT/tests/lib/binary_freshness.sh"
zs_check_artifact_freshness "$ROOT" "sdks/vite-plugin/dist/index.js" \
  "sdks/vite-plugin/src" || {
    rc=$?
    [ "$rc" = "2" ] && { fail "vite-plugin freshness check could not run"; exit 2; }
    fail "sdks/vite-plugin/dist is stale (ZS_FRESHNESS_STRICT=1)"; exit 1;
  }

# The state-dir-lock marker is declared once in Rust and once in TypeScript, and
# the dev banner picks between two contradictory remedies based on the TS copy.
# Both crate suites pass if only one spelling is edited - neither can see the
# other - so this is the only comparison. Exits 2 when it cannot find a
# declaration, because a grep that matches nothing is how a gate goes quietly
# green. Self-test: tests/lib/state_dir_lock_marker.sh --selftest
bash "$ROOT/tests/lib/state_dir_lock_marker.sh" || {
  rc=$?
  [ "$rc" = "2" ] && { fail "state-dir-lock marker gate could not run"; exit 2; }
  fail "state-dir-lock marker differs between Rust and TypeScript"; exit 1;
}

# NOT covered here, and worth knowing: the RUST binaries this script starts are
# unchecked. `zs_check_binary_freshness` is the function for that and takes the
# binary directory plus names; wiring it needs this script's binary list, which
# is a separate change rather than an oversight to fix silently.

# --- 1. Build the starter (real vite-plugin → .zship) ---
step 1 "Build examples/starter (pnpm build → dist/app.zship)"
( cd "$STARTER" && pnpm build ) >/tmp/gp-build.log 2>&1
[ -f "$ZSHIP" ] && pass "built $(basename "$ZSHIP") ($(du -k "$ZSHIP" | cut -f1)KB)" || { fail "build produced no app.zship"; tail -20 /tmp/gp-build.log; exit 1; }

# --- 1b. The artifact is INSPECTABLE, and declares the procedures we shipped ---
#
# There is no `zeroship inspect` command (removed in the artifact-layout
# redesign), so the only way a creator sees inside their own build is plain
# `tar`. That makes the archive layout a creator-facing contract even though no
# first-party command depends on it: change it, and the only available way to
# answer "what are my actual wire ids" stops working.
#
# That question is not hypothetical. Wire ids are configurable and need not
# match export names - `examples/db-todos` exports `seedUser` but publishes
# `users.seed`, and calling the export name returns a bare
# `Method not found: seedUser` with no hint that an id mapping exists. The
# manifest is where the real answer lives.
#
# WHAT THIS DOES NOT CATCH, and it is the failure that has actually bitten:
# a module missing its `"use server"` directive builds clean, reports 0 server
# functions, and STILL emits a manifest declaring every RPC - so a manifest
# listing `getMessages` is NOT evidence that `getMessages` is callable. This
# asserts the artifact is readable and says what we expect; step 5 is what
# proves a procedure actually answers.
zship_rpc_ids() {
  local mf
  mf="$(tar --zstd -xOf "$1" manifest.json 2>/dev/null)" || return 3
  [ -n "$mf" ] || return 3
  printf '%s' "$mf" | node -e '
    let s=""; process.stdin.on("data",d=>s+=d).on("end",()=>{
      let m; try { m = JSON.parse(s); } catch { process.exit(4); }
      const ids = Object.keys(m.resources||{}).filter(k=>k.startsWith("rpc:")).sort();
      if (!ids.length) process.exit(5);
      process.stdout.write(ids.join(","));
    });'
}
GP_IDS="$(zship_rpc_ids "$ZSHIP")"; GP_IDS_RC=$?
if [ "$GP_IDS_RC" -ne 0 ]; then
  fail "manifest not readable from the .zship (rc=$GP_IDS_RC: 3=no manifest.json, 4=unparseable, 5=no rpc resources)"
elif [ "$GP_IDS" = "rpc:addMessage,rpc:boom,rpc:getMessages" ]; then
  pass "artifact inspectable via tar; manifest declares $GP_IDS"
else
  fail "manifest rpc ids changed: expected rpc:addMessage,rpc:boom,rpc:getMessages got '$GP_IDS'"
fi

# --- 2. Bring up the stack (control + migrated + worker + gateway) ---
#
# FOUR services since step 10 landed, not three. The scaffold template uses
# env.db, env.storage and env.kv, so measuring its deployed behaviour needs the
# migration service (to create the creator's schema the way a real deploy does)
# and the worker's --db/--kv-url/--storage-url. Without them the deployed side
# is crippled by the harness, and every divergence step 10 reports would be the
# harness's, not the platform's.
step 2 "Bring up the stack"
for p in $CONTROL_PORT $WORKER_PORT $GATE_PORT $MIGRATED_PORT; do lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done
rm -rf /tmp/gp-bundles /tmp/gp-storage /tmp/gp-migrated-tmp
mkdir -p /tmp/gp-storage

# Fresh dedicated DB + the full platform schema (db/migrations-ts JS DSL,
# recorded to transient IR by zeroship-platform-migrate).
echo "  migrating a fresh $PG_DB ..."
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "DROP DATABASE IF EXISTS $PG_DB WITH (FORCE)" >/dev/null 2>&1 || \
  docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "DROP DATABASE IF EXISTS $PG_DB" >/dev/null 2>&1 || true
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "CREATE DATABASE $PG_DB" >/dev/null 2>&1 || true
"$BIN/zeroship-platform-migrate" \
    --database-url "$DB_URL" \
    --migrations-dir "$ROOT/db/migrations-ts" \
    --project-schema zeroship \
    --project-id zeroship >/tmp/gp-migrate.log 2>&1 \
    || { fail "platform migrations failed"; tail -20 /tmp/gp-migrate.log; exit 1; }
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -tAc "select to_regclass('zeroship.apps')" 2>/dev/null | grep -q apps \
  && pass "schema migrated (fresh $PG_DB)" || { fail "schema missing after migrate"; exit 1; }

# A least-privilege role must hold every privilege its own cron actually uses.
#
# crates/auth/src/cron/token_sweep.rs:125-128 DELETEs from
# zeroship.token_revocations. zeroship_auth was granted only select/insert/update
# on it (the 20260702000900_grants.ts grant that also covers oauth_grants and
# oauth_clients), so every sweep failed and the table grew without bound. Fixed
# by 20260811000000_auth_token_revocations_delete.ts.
#
# MEASURED both ways on a scratch database, one variable:
#   migrations WITH that file    -> DELETE = true
#   migrations WITHOUT that file -> DELETE = false
# so this assertion is load-bearing rather than green by construction. The same
# query returned false against the live Supabase deployment before the fix.
#
# WHAT THIS DOES NOT CATCH: privilege, not behaviour. It proves the GRANT
# exists, not that the sweep runs, deletes the right rows, or is scheduled. A
# sweep that never fires still satisfies it.
# SELECT is checked alongside DELETE for the reason the control-side sweep check
# below states in full: the statement is
#   DELETE FROM zeroship.token_revocations WHERE revoked_after < NOW() - ...
# (crates/authz/src/wrapper_revocation.rs, reached from
# crates/auth/src/cron/token_sweep.rs:125) and PostgreSQL requires SELECT on any
# column named in the WHERE clause. This assertion tested DELETE ALONE until
# 2026-08-12, so a migration granting DELETE without SELECT would have broken the
# sweep exactly as #319 did and left this line green. Both privileges are granted
# today - measured, so this is a latent hole closed, not a live defect fixed.
#
# BOTH ARMS PROVEN LOAD-BEARING against the live schema, no DB mutation needed for
# the first: zeroship_auth on zeroship.email_suppressions is select=t delete=f and
# yields 1. For the second no shipped pair grants DELETE without SELECT, so it was
# built: a scratch table with only DELETE granted also yielded 1, then dropped.
auth_del=$(docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -tAc \
  "select count(*) from pg_class c join pg_namespace n on n.oid=c.relnamespace cross join (values ('SELECT'),('DELETE')) p(v) where n.nspname='zeroship' and c.relname='token_revocations' and has_table_privilege('zeroship_auth',c.oid,p.v)" 2>/dev/null | tr -d '[:space:]')
[ "$auth_del" = "2" ] \
  && pass "zeroship_auth holds SELECT+DELETE on token_revocations (its sweep needs both)" \
  || fail "zeroship_auth token_revocations privileges read $auth_del of 2: token_sweep cannot succeed (#319)"

# The SAME table, the GATEWAY role, and a DIFFERENT missing privilege - which is
# why the assertion above could not see this one. #319 was auth needing DELETE;
# this is the gateway needing UPDATE.
#
# crates/authz/src/wrapper_revocation.rs:41 revoke_family() is
#   INSERT INTO zeroship.token_revocations (...) VALUES (...)
#   ON CONFLICT (client_id, sub) DO UPDATE SET revoked_after = EXCLUDED.revoked_after
# called from crates/gateway/src/backchannel_logout.rs:508 and
# browser_auth.rs:421, both production (router/auth.rs's #[cfg(test)] starts at
# 1071, and neither of these files is that one). PostgreSQL requires UPDATE to
# PLAN an ON CONFLICT DO UPDATE, so a missing UPDATE fails EVERY call, not just
# the conflicting ones - and both call sites swallow the error, so signout still
# answers 204 while the marker was never written.
#
# MEASURED 2026-08-12 as zeroship_gateway inside BEGIN ... ROLLBACK, one variable:
#   INSERT ... VALUES (...)                        -> INSERT 0 1
#   INSERT ... VALUES (...) ON CONFLICT DO UPDATE  -> ERROR: permission denied
# so the ON CONFLICT clause is the cause, not the table or the row.
#
# WHAT THIS DOES NOT CATCH: privilege, not behaviour. It proves the GRANT exists,
# not that signout calls revoke_family, nor that the marker is later read by the
# per-request gate. A signout path that stopped calling it would still pass.
gw_revoke=$(docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -tAc \
  "select count(*) from pg_class c join pg_namespace n on n.oid=c.relnamespace cross join (values ('SELECT'),('INSERT'),('UPDATE')) p(v) where n.nspname='zeroship' and c.relname='token_revocations' and has_table_privilege('zeroship_gateway',c.oid,p.v)" 2>/dev/null | tr -d '[:space:]')
[ "$gw_revoke" = "3" ] \
  && pass "zeroship_gateway holds SELECT+INSERT+UPDATE on token_revocations (revoke_family upserts)" \
  || fail "zeroship_gateway token_revocations privileges read $gw_revoke of 3: revoke_family cannot succeed, so signout never revokes the family (#356)"

# The same class, on the control side, for an audit trail that is WRITTEN rather
# than only swept.
#
# crates/authz/src/eval.rs:243 INSERTs into zeroship.authz_decisions from
# enforce(), on every authorization decision. control is the only caller of
# enforce() among the services in the compose stack. That table had NO grant to
# any zeroship_* role, and the insert error is swallowed
# (`if let Err(err) = ... { tracing::error!(...) }`), so under least privilege
# every decision failed to record with no functional symptom at all. Fixed by
# 20260811000200_control_audit_grants.ts.
#
# MEASURED before that migration, as zeroship_control against a live session:
#   insert into zeroship.authz_decisions ... -> ERROR: permission denied
#   insert into zeroship.app_audit ...       -> INSERT 0 1
# one variable, opposite outcomes.
authz_write=$(docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -tAc \
  "select has_table_privilege('zeroship_control','zeroship.authz_decisions','INSERT')" 2>/dev/null | tr -d '[:space:]')
[ "$authz_write" = "t" ] \
  && pass "zeroship_control can INSERT authz_decisions (every authz decision writes one)" \
  || fail "zeroship_control lacks INSERT on authz_decisions: the authorization audit trail is silently never written (#325)"

# control's retention sweep reads occurred_at and deletes from BOTH audit tables
# (crates/control/src/cron/audit_retention.rs, sweep_all -> delete_older_than).
# PostgreSQL requires SELECT on any column named in the WHERE clause, so DELETE
# alone is not enough; the working reference, zeroship_auth on audit_events,
# holds select/insert/delete for exactly this reason.
sweep_priv=$(docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -tAc \
  "select count(*) from pg_class c join pg_namespace n on n.oid=c.relnamespace cross join (values ('SELECT'),('DELETE')) p(v) where n.nspname='zeroship' and c.relname in ('app_audit','authz_decisions') and has_table_privilege('zeroship_control',c.oid,p.v)" 2>/dev/null | tr -d '[:space:]')
[ "$sweep_priv" = "4" ] \
  && pass "zeroship_control holds SELECT+DELETE on both audit tables (its retention sweep needs both)" \
  || fail "zeroship_control audit retention privileges read $sweep_priv of 4: the sweep cannot run (#324)"

# Same class again, and this one was found by RUNNING control under its real role
# rather than by reading grants.
#
# zeroship.connect_checkout_failures had no grant to any role, and control uses
# it from two production paths: cron/billing_notify.rs:374 SELECTs it as one of
# seven union sources, and stripe_store.rs:420 INSERTs into it. The SELECT is
# unguarded, so the ENTIRE billing-notify tick failed every run -- no billing
# notification of any kind was produced, including the six kinds sourced from
# tables the role reads fine. Fixed by 20260811000300_control_connect_failures_grant.ts.
#
# MEASURED, one variable (the DSN), same binary and flags:
#   control as zeroship_control -> ERROR "control billing-notify tick failed"
#                                        error="database: db error"
#   control as postgres         -> no error at all
# and after the grant, the least-privilege boot logs zero ERROR lines.
#
# WHAT THIS DOES NOT CATCH: privilege, not behaviour, and only for the two
# tables named. It proves the grant exists, not that billing_notify produces
# correct notifications. It would also not have FOUND this -- only running a
# service under its real role does that, which is still not done anywhere
# (#322).
connect_priv=$(docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -tAc \
  "select count(*) from (values ('SELECT'),('INSERT')) p(v) where has_table_privilege('zeroship_control','zeroship.connect_checkout_failures',p.v)" 2>/dev/null | tr -d '[:space:]')
[ "$connect_priv" = "2" ] \
  && pass "zeroship_control can read+write connect_checkout_failures (billing_notify and stripe_store both use it)" \
  || fail "zeroship_control connect_checkout_failures privileges read $connect_priv of 2: the whole billing-notify tick fails (#324)"

# Granting that DELETE must NOT weaken the append-only invariant.
#
# Immutability on these tables is enforced by BEFORE DELETE/UPDATE/TRUNCATE
# triggers running <table>_block_tamper(), which refuse even the superuser. The
# trigger permits a DELETE only for a session that has opted in with
# `SET zeroship.audit_retention = 'on'` - which is what the retention cron does
# on its own dedicated connection. This is the two-sided check: the same DELETE
# must be REFUSED without the opt-in and ACCEPTED with it. A grant that
# accidentally bypassed the trigger, or a trigger that stopped discriminating,
# fails one arm or the other.
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -tAc \
  "insert into zeroship.app_audit (app_id, action) values (gen_random_uuid(),'golden_tamper_probe')" >/dev/null 2>&1
tamper_refused=$(docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -tAc \
  "delete from zeroship.app_audit where action='golden_tamper_probe'" 2>&1 | grep -c "append-only")
tamper_allowed=$(docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -tAc \
  "set zeroship.audit_retention = 'on'; delete from zeroship.app_audit where action='golden_tamper_probe'" 2>&1 | grep -c "DELETE 1")
[ "$tamper_refused" = "1" ] && [ "$tamper_allowed" = "1" ] \
  && pass "app_audit stays append-only: DELETE refused without the retention GUC, accepted with it" \
  || fail "audit tamper guard not discriminating (refused=$tamper_refused allowed=$tamper_allowed, both must be 1): the append-only trigger no longer gates on zeroship.audit_retention (#324)"

# Platform tables must come from migrations, not from a service doing DDL.
#
# The workflow scheduler store was created at runtime by
# crates/workflow-scheduler/src/store.rs provision_sql(), which control called on
# every tick. Its first statement is `CREATE SCHEMA IF NOT EXISTS`, and Postgres
# checks database-level CREATE BEFORE the existence short-circuit, so it fails
# under any least-privilege role even when the schema is already there. MEASURED
# against the live deployment before 20260811000100_workflow_scheduler_store.ts:
# CREATE privilege false, schema absent, 56 tick ERRORs in 60 seconds, every one
# SQLSTATE 42501.
#
# The tables sit in `zeroship`, not a schema of their own: the platform migration
# charter admits exactly ["public", "zeroship"], and a first draft creating a
# `workflow_scheduler` schema was refused at lower time with CROSS_SCHEMA.
#
# Asserted BEFORE any service starts, on purpose. Run after control boots and it
# would pass for the wrong reason -- this harness hands control a privileged DSN,
# so the old runtime DDL succeeds here and hides a defect that only appears under
# a restricted role.
#
# The columns are checked, not just the table names, because the migration and
# the Rust provision_sql() are two spellings of the same objects and can drift;
# store.rs reads these columns by name.
#
# WHAT THIS DOES NOT CATCH: it says nothing about whether workflows RUN. Empty
# tables with the right columns satisfy it. It also does not check the indexes.
sched_cols=$(docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -tAc \
  "select count(*) from information_schema.columns where table_schema='zeroship' and ((table_name='workflow_scheduler_timers' and column_name in ('run_id','app_id','wake_at','generation','registered_at')) or (table_name='workflow_scheduler_inflight' and column_name in ('run_id','app_id','deadline','dispatch_generation','dispatched_at')))" 2>/dev/null | tr -d '[:space:]')
[ "$sched_cols" = "10" ] \
  && pass "workflow scheduler store built by migrations (10/10 columns)" \
  || fail "workflow scheduler store missing after migrate: $sched_cols of 10 columns - control cannot CREATE SCHEMA under its own role, so env.workflows is dead (#320)"

sched_dml=$(docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -tAc \
  "select count(*) from pg_class c join pg_namespace n on n.oid=c.relnamespace cross join (values ('SELECT'),('INSERT'),('UPDATE'),('DELETE')) p(v) where n.nspname='zeroship' and c.relname in ('workflow_scheduler_timers','workflow_scheduler_inflight') and has_table_privilege('zeroship_control',c.oid,p.v)" 2>/dev/null | tr -d '[:space:]')
[ "$sched_dml" = "8" ] \
  && pass "zeroship_control holds the scheduler store DML (8/8)" \
  || fail "zeroship_control lacks scheduler store privileges: $sched_dml of 8 - the tick would fail even with the tables present (#320)"

# The service roles must NOT be able to do DDL. This is the property #319, #320
# and #321 all live in, and the one a future privilege error is most likely to be
# "fixed" by relaxing.
#
# It matters because the cheapest repair for "permission denied for database" is
# a GRANT, and that silently converts the platform back into one where any
# service can create arbitrary schemas and roles. #320 declined exactly that and
# moved the tables into migrations instead; this keeps the decision from being
# quietly reversed.
#
# SELF-TESTING, which is the point of including postgres. The first two readings
# are the property; the third proves the query can return true at all, so a
# version of this check that is broken (wrong role name, wrong database, a typo
# that always yields false) fails instead of passing vacuously. MEASURED
# 2026-08-11 on the live deployment: postgres create=true createrole=true, and
# zeroship_control / zeroship_gateway / zeroship_auth all false/false.
#
# Confirmed in TWO clusters, which matters because they differ in the one way
# that could have made the reading accidental: on the managed deployment
# `postgres` is NOT a superuser (rolsuper=false, create=true), and on the local
# compose cluster it IS. The guard reads 3 in both, so it depends on neither
# shape. The local reading came from a database built FROM SCRATCH by
# zeroship-platform-migrate (624 ops, exit 0), where all four privilege
# assertions above read 10 / 8 / t / 3.
#
# WHAT THIS DOES NOT CATCH: it reads the roles' privileges, not what the services
# actually do with them. It cannot see a service that connects as postgres
# anyway, which is what every harness in tests/ does today -- see #322.
ddl_guard=$(docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -tAc \
  "select (case when has_database_privilege('zeroship_control',current_database(),'CREATE') then 0 else 1 end)
        + (case when (select rolcreaterole from pg_roles where rolname='zeroship_control') then 0 else 1 end)
        + (case when has_database_privilege('postgres',current_database(),'CREATE') then 1 else 0 end)" 2>/dev/null | tr -d '[:space:]')
[ "$ddl_guard" = "3" ] \
  && pass "zeroship_control holds no DDL privilege, and the probe discriminates (3/3)" \
  || fail "DDL privilege guard reads $ddl_guard of 3: control gained CREATE or CREATEROLE, or the probe stopped discriminating (#320/#321)"

# Ephemeral Redis for `env.kv`. The worker leaves the namespace ABSENT when
# --kv-url is empty (crates/worker/src/main.rs), by design -- so an app calling
# @zeroship/kv fails loudly rather than diverging silently. Step 10's app calls
# it, so the harness has to supply one.
docker rm -f "$REDIS_CONTAINER" >/dev/null 2>&1 || true
docker run --name "$REDIS_CONTAINER" -d -p "$REDIS_PORT:6379" redis:7-alpine >/dev/null 2>&1
for _ in $(seq 1 30); do docker exec "$REDIS_CONTAINER" redis-cli ping 2>/dev/null | grep -q PONG && break; sleep 1; done
docker exec "$REDIS_CONTAINER" redis-cli ping 2>/dev/null | grep -q PONG \
  && pass "ephemeral Redis on :$REDIS_PORT (env.kv backend)" \
  || { fail "Redis never became ready on :$REDIS_PORT"; exit 1; }

# One ed25519 key shared by control and migrated: step 10 mints an offline
# platform-admin PAT signed with it to call migrated's apply endpoint. Pass it
# to BOTH or the PAT verifies against a key the service never saw and every
# call comes back 401 "platform token verification failed".
GP_SIGNING_KEY=/tmp/gp-signing-key.pem
openssl genpkey -algorithm ed25519 -out "$GP_SIGNING_KEY" 2>/dev/null
chmod 600 "$GP_SIGNING_KEY"

# `--workers` is NOT optional decoration, and its absence was invisible for as
# long as this harness existed. crates/control/src/main.rs:81 declares it with
# `default_value = "http://localhost:8080"`, and this harness runs its worker on
# $WORKER_PORT (8390). Every other path here goes gateway -> worker and the
# GATEWAY is told the URL explicitly, so control's own worker list had never
# been exercised by anything -- until step 13 asked control to fetch app logs
# and got `502 {"error":"internal error"}` with
# `worker log fetch failed ... "error":"parse logs JSON: EOF"` in its log,
# because it was reading an empty body from a port nothing listens on.
"$BIN/zeroship-control" --port "$CONTROL_PORT" --db "$DB_URL" --blob-store /tmp/gp-bundles \
  --workers "http://localhost:$WORKER_PORT" \
  --control-key "$CONTROL_KEY" --master-key "$MASTER_KEY" \
  --signing-key-file "$GP_SIGNING_KEY" >/tmp/gp-control.log 2>&1 & PIDS+=($!)
"$BIN/zeroship-migrated" --port "$MIGRATED_PORT" --db "$DB_URL" --provision-db "$DB_URL" \
  --signing-key-file "$GP_SIGNING_KEY" --tmp-dir /tmp/gp-migrated-tmp \
  --dev-insecure >/tmp/gp-migrated.log 2>&1 & PIDS+=($!)
sleep 3
# ONE definition of the worker command line, because there are TWO places that
# start it: here, and step 8, which kills it to measure the runtime-unavailable
# path. Those two drifting apart is not hypothetical -- step 8 killed the worker
# and never restarted it at all, so every step after it ran against a dead
# worker. Step 9 is dev-only and never noticed; step 10 drives the DEPLOYED tier
# and would have reported the gateway's 502 as a platform divergence.
#
# --db/--kv-url/--storage-url: without them env.db / env.kv / env.storage are
# ABSENT on the deployed tier and step 10's app would fail for a reason that
# has nothing to do with what it is measuring.
gp_start_worker() {
  "$BIN/zeroship-worker" --port "$WORKER_PORT" --worker-threads 2 --control "http://localhost:$CONTROL_PORT" \
    --control-key "$CONTROL_KEY" --db "$DB_URL" --kv-url "redis://127.0.0.1:$REDIS_PORT" \
    --storage-url /tmp/gp-storage \
    --blob-store /tmp/gp-bundles --poll-interval 2 >>/tmp/gp-worker.log 2>&1 &
  WORKER_PID=$!
  PIDS+=($WORKER_PID)
  local i
  for i in $(seq 1 30); do
    curl -sf "http://localhost:$WORKER_PORT/health" >/dev/null 2>&1 && return 0
    sleep 1
  done
  return 1
}
: >/tmp/gp-worker.log
gp_start_worker
sleep 2
# The gateway refuses to boot without a broker secret; it signs the RP-initiated
# login handshake, so there is no safe default and no dev fallback.
GATE_BROKER_SECRET=/tmp/gp-gate-broker-secret
openssl rand -base64 48 > "$GATE_BROKER_SECRET"
chmod 600 "$GATE_BROKER_SECRET"
# THIS GATEWAY HAS NO DATABASE, and that is a supported mode rather than an
# omission: crates/gateway/src/main.rs:619-622 accepts an empty DSN "for dev /
# smoke modes that don't exercise the OIDC RP path", and its db-backed handlers
# "gracefully return 401 when `db` is None instead of panicking".
#
# Harmless today, MEASURED: the only paths this file requests from the gateway
# are /health and /apps/<name>/... dispatch and assets - seven distinct paths,
# enumerated 2026-08-11, none of them on the db-backed auth surface. Scenario 6
# is walked by tests/e2e_dev_vs_deployed_auth.sh, which runs a real OP.
#
# THE TRAP IS FOR WHOEVER ADDS THE NEXT ASSERTION. Anything here that touches
# __zeroship/auth/*, the session exchange, backchannel logout or the anchor /
# revocation paths would measure the db-is-None 401 rather than the real
# behaviour, and would pass while proving nothing. Give the gateway a DSN first
# if you need those, and check that both tiers actually reach the database.
# `--db` is the SECOND half of the same prerequisite, and the key alone is inert
# without it. The gateway with no DSN sets `db: None` (main.rs:630-642), logs
# that session validation is disabled, and then 401s every auth-gated request -
# the SAME observable as no signing key, as a malformed `pws_` subject, and as
# the wrong cookie name. Four distinct causes, one symptom; that is why each was
# cleared separately rather than together. Fail-CLOSED, not a hole.
#
# PASS IT EXPLICITLY EVEN THOUGH THE ARG HAS AN ENV FALLBACK, which is the part
# worth knowing. main.rs:78 declares `#[arg(long = "db", env = "DATABASE_URL")]`,
# so an ambient DATABASE_URL configures the gateway just as well - and this
# harness never sets that variable (line 64 only READS it as a default for
# $DB_URL). So before this flag, whether the gateway could validate a session
# depended on the operator's shell. MEASURED 2026-08-12, three arms one variable
# apart, `zeroship-gate` under ZEROSHIP_DEV_INSECURE=1:
#     no --db, DATABASE_URL unset  -> "session validation disabled"
#     --db "$DB_URL"               -> "pg connection pool configured"
#     no --db, DATABASE_URL set    -> "pg connection pool configured"
# The third arm is why "the flag is required" would have been too strong: the
# flag makes the DSN deterministic and equal to the one this harness itself
# uses, rather than inherited from the environment. Ticket #327 names the same
# prerequisite as "a gateway arm still needs a DSN first".
#
# `--signing-key-file` shares the SAME ed25519 key control and migrated already
# use ($GP_SIGNING_KEY). Without it the gateway has no key to verify an app
# session cookie against, so every authenticated request is anonymous and any
# `auth: "user"` procedure answers 401 - which is indistinguishable from a
# working gate refusing a bad credential, and is why scoped-data coverage could
# not be written here before.
#
# THIS HARNESS IS THE ONLY VEHICLE for that coverage, and the reason is
# structural rather than a preference. Measured 2026-08-12: tests/lib/e2e_stack.sh
# runs `zeroship-platform-migrate` only - the PLATFORM schema - and never starts
# `migrated`, so a creator app's own migration is never applied there and its
# table does not exist. golden_path starts control + migrated + worker + gateway,
# so it is the only harness where a creator's rows and the gateway's identity
# derivation are both live at once.
"$BIN/zeroship-gate" --port "$GATE_PORT" --control "http://localhost:$CONTROL_PORT" \
  --control-key "$CONTROL_KEY" --workers "http://localhost:$WORKER_PORT" --blob-store /tmp/gp-bundles \
  --gateway-broker-secret-file "$GATE_BROKER_SECRET" --poll-interval 2 \
  --signing-key-file "$GP_SIGNING_KEY" --db "$DB_URL" >/tmp/gp-gate.log 2>&1 & PIDS+=($!)
sleep 3

curl -sf "http://localhost:$CONTROL_PORT/health" >/dev/null && pass "control healthy" || { fail "control down"; tail -20 /tmp/gp-control.log; exit 1; }
curl -sf "http://localhost:$WORKER_PORT/health"  >/dev/null && pass "worker healthy"  || { fail "worker down";  tail -20 /tmp/gp-worker.log; exit 1; }
curl -sf "http://localhost:$GATE_PORT/health"    >/dev/null && pass "gateway healthy" || { fail "gateway down"; tail -20 /tmp/gp-gate.log; exit 1; }
curl -sf "http://localhost:$MIGRATED_PORT/health" >/dev/null && pass "zeroship-migrated healthy" || { fail "migrated down"; tail -20 /tmp/gp-migrated.log; exit 1; }

# --- 2b. The OPERATOR config seam: control's DSN through the [secrets] overlay ---
#
# Step 2 hands control its DSN on the COMMAND LINE (`--db "$DB_URL"`), which is
# the one tier that always worked. Every deployed zeroship stack uses the other
# one: deploy/ops/zeroship.toml carries
# `[secrets] database_url = "urn:zeroship:env:ZEROSHIP_DATABASE_URL"` and the
# compose services set that env var, so the DSN arrives through the FILE tier
# and `DATABASE_URL` is never set at all. Nothing in this harness -- or in any
# crate suite, which cannot see a config file it does not mount -- exercised
# that tier, and the tier was broken: `--db` carried a non-empty clap
# `default_value`, `obtain_secret` takes its CLI branch on ANY non-empty string,
# so the compiled default occupied the CLI tier and the file reference was never
# consulted. Control dialled `postgres://localhost/zeroship` inside its own
# container, panicked in `Registry::new`, and crash-looped forever while every
# other binary on the same network connected with the same credentials.
#
# The three assertions are not three ways of saying one thing:
#   A. the file tier RESOLVES  - overlay ref + env var, no --db, no DATABASE_URL
#   B. the overlay was READ    - so a green A cannot be explained by some other
#                                tier having quietly supplied a working DSN
#   C. CLI still BEATS file    - the fix moves a default out of the CLI tier; if
#                                it had inverted the precedence instead, A would
#                                still be green and only this would catch it
step 2b "Operator config: control resolves its DSN from the [secrets] overlay"
CFG_PORT="${CFG_PORT:-9391}"
CFG_PORT_B="${CFG_PORT_B:-9392}"
for p in $CFG_PORT $CFG_PORT_B; do lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done
GP_OVERLAY=/tmp/gp-overlay.toml
cat > "$GP_OVERLAY" <<TOML
[secrets]
database_url = "urn:zeroship:env:GP_OVERLAY_DB_URL"
TOML

# `env -u DATABASE_URL` is load-bearing: with it set, the CLI tier legitimately
# wins and this step would measure nothing.
env -u DATABASE_URL GP_OVERLAY_DB_URL="$DB_URL" \
  "$BIN/zeroship-control" --port "$CFG_PORT" --config "$GP_OVERLAY" \
  --blob-store /tmp/gp-bundles --control-key "$CONTROL_KEY" --master-key "$MASTER_KEY" \
  --signing-key-file "$GP_SIGNING_KEY" >/tmp/gp-control-overlay.log 2>&1 & PIDS+=($!)
for _ in $(seq 1 20); do curl -sf "http://localhost:$CFG_PORT/health" >/dev/null 2>&1 && break; sleep 1; done
if curl -sf "http://localhost:$CFG_PORT/health" >/dev/null 2>&1; then
  pass "control boots with its DSN from [secrets] database_url (no --db, no DATABASE_URL)"
else
  fail "control did not come up on the [secrets] overlay DSN"
  tail -5 /tmp/gp-control-overlay.log
fi
grep -q "loaded overlay" /tmp/gp-control-overlay.log \
  && pass "the generated overlay was actually read (control logged it)" \
  || fail "control never logged loading $GP_OVERLAY - assertion A proves nothing"

# One variable changed against the run above: an explicit --db, deliberately
# unreachable, while the overlay still names a WORKING DSN. CLI must win, so
# this must NOT come up.
env -u DATABASE_URL GP_OVERLAY_DB_URL="$DB_URL" \
  "$BIN/zeroship-control" --port "$CFG_PORT_B" --config "$GP_OVERLAY" \
  --db "postgres://postgres:zeroship@127.0.0.1:1/$PG_DB" \
  --blob-store /tmp/gp-bundles --control-key "$CONTROL_KEY" --master-key "$MASTER_KEY" \
  --signing-key-file "$GP_SIGNING_KEY" >/tmp/gp-control-cliwins.log 2>&1 & PIDS+=($!)
sleep 6
if curl -sf "http://localhost:$CFG_PORT_B/health" >/dev/null 2>&1; then
  fail "an explicit --db was ignored in favour of the [secrets] overlay (precedence inverted)"
else
  pass "an explicit --db still beats the [secrets] overlay"
fi
# "not healthy after 6s" is ALSO what an unrelated slow start, a busy machine or
# a binary that died on some earlier config error looks like, so the check above
# cannot tell "CLI won" from "control never got that far". This one requires the
# POSITIVE evidence: control must have tried the CLI DSN and failed on it. Both
# must hold; a green pair is the only reading that means precedence held.
grep -q "failed to connect to database" /tmp/gp-control-cliwins.log \
  && pass "control demonstrably tried the unreachable --db DSN and failed on it" \
  || fail "control did not log a connect failure - it never reached the CLI DSN, so the assertion above is vacuous"
for p in $CFG_PORT $CFG_PORT_B; do lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done

# --- 2c. The same control, under its REAL role ---
step 2c "Least privilege: control under zeroship_control matches control under postgres"
# Every other step in this file runs control as the SUPERUSER (DB_URL at the top
# is postgres://postgres:...). That is why six defects of one class shipped past
# a suite that reads as thorough: as superuser they all succeed.
#   #319 auth token_sweep could never DELETE
#   #320 control's workflow tick could never CREATE SCHEMA
#   #321 the per-app journal can never CREATE ROLE/SCHEMA
#   #324 audit retention could not SELECT/DELETE, authz_decisions could not INSERT
#   #324 connect_checkout_failures could not be read, killing the whole
#        billing-notify tick -- found by RUNNING this comparison, not by reading
#
# THE ASSERTION IS THE DIFF, not a message grep. Boot the same binary with the
# same flags twice, changing ONLY the DSN, and require the two arms to log the
# same number of ERROR lines. Keying on a message would have missed the defect
# that motivated this: its text was "database: db error", which names neither a
# table nor a privilege. The diff catches the next one whatever it says, and it
# self-normalises against unrelated errors, which hit both arms equally.
#
# RED/GREEN PROVEN BY ME, by revoking the grant landed in a497fcbdf and
# restoring it, same script both times:
#   grant revoked  -> leastpriv_errors=1 superuser_errors=0   (this step FAILS)
#   grant restored -> leastpriv_errors=0 superuser_errors=0   (this step PASSES)
#
# WHAT THIS DOES NOT CATCH: only control, and only the paths its crons touch in
# the seconds this runs. Gateway, auth and worker are untouched, and a cron on a
# long cadence will not have ticked. It is a floor on the class, not a proof
# that the least-privilege deployment is sound.
LP_PORT_A=9401; LP_PORT_B=9402
# Derive the restricted DSN from the harness's own, so pointing the harness at a
# different host stays consistent. Passwords equal role names today (#326); if
# that is fixed, this needs GOLDEN_LP_DSN.
LP_DSN="${GOLDEN_LP_DSN:-$(printf '%s' "$DB_URL" | sed 's|//postgres:[^@]*@|//zeroship_control:zeroship_control@|')}"
if [ "$LP_DSN" = "$DB_URL" ]; then
  fail "could not derive a zeroship_control DSN from DB_URL - set GOLDEN_LP_DSN explicitly rather than letting this step compare a role against itself"
else
  for _spec in "lpA:$LP_DSN:$LP_PORT_A" "lpB:$DB_URL:$LP_PORT_B"; do
    _tag="${_spec%%:*}"; _rest="${_spec#*:}"; _dsn="${_rest%:*}"; _port="${_rest##*:}"
    "$BIN/zeroship-control" --port "$_port" --db "$_dsn" --blob-store /tmp/gp-bundles \
      --control-key "$CONTROL_KEY" --master-key "$MASTER_KEY" \
      --signing-key-file "$GP_SIGNING_KEY" > "/tmp/gp-$_tag.log" 2>&1 &
    # `disown`, and deliberately NOT PIDS+=. These two are killed a few lines
    # below, inside this step. Registering them for the EXIT trap as well made
    # bash report `line NNN: <pid> Killed` when it reaped them, and because the
    # reap is asynchronous one of those notices landed in the MIDDLE of step 3's
    # output - a line naming a step it has nothing to do with, in a harness
    # whose whole value is a readable pass/fail list. Measured in the first full
    # run of this step, not predicted.
    disown $! 2>/dev/null || true
  done
  # Both must actually serve; comparing the logs of two processes that never
  # started would report 0 == 0 and read as a pass.
  lp_up=0
  for _i in $(seq 1 25); do
    a=$(curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:$LP_PORT_A/health" 2>/dev/null)
    b=$(curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:$LP_PORT_B/health" 2>/dev/null)
    [ "$a" = "200" ] && [ "$b" = "200" ] && { lp_up=1; break; }
    sleep 1
  done
  [ "$lp_up" = "1" ] \
    && pass "control serves under zeroship_control as well as under postgres" \
    || fail "control did not come up on both DSNs (leastpriv=$a superuser=$b) - the comparison below cannot mean anything"
  if [ "$lp_up" = "1" ]; then
    sleep 8   # let the fast crons tick at least once
    lp_err=$(grep -c '"level":"ERROR"' /tmp/gp-lpA.log || true)
    su_err=$(grep -c '"level":"ERROR"' /tmp/gp-lpB.log || true)
    [ "$lp_err" = "$su_err" ] \
      && pass "least-privilege control logs the same $lp_err error(s) as the superuser control" \
      || fail "least-privilege control logs $lp_err error(s) vs the superuser's $su_err: a privilege the code uses is not granted (the #319 class) - see /tmp/gp-lpA.log"
  fi
  for p in $LP_PORT_A $LP_PORT_B; do lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done
fi

# --- 3. Create app + deploy the real .zship ---
step 3 "Create app + deploy"
TOKEN="${ZEROSHIP_TOKEN:-}"
if [ -n "$TOKEN" ]; then
  # Scoped prerequisite: jq is used ONLY on this arm, so it is checked here
  # rather than in the top-of-file list. Without it the two `jq -r` calls below
  # print nothing, APP_ID comes out empty, and the harness reports
  # "create app: <the full JSON body>" - a message that points at the server
  # response when the actual fault is a missing tool on this machine.
  command -v jq >/dev/null 2>&1 || {
    echo "FAIL: ZEROSHIP_TOKEN is set, which selects the PAT arm, but jq is not installed." >&2
    echo "      Install jq, or unset ZEROSHIP_TOKEN to use the dev-provision arm." >&2
    exit 2
  }
  APP=$(curl -sf -X POST "http://localhost:$CONTROL_PORT/api/apps" -H 'Content-Type: application/json' \
    -H "Authorization: Bearer $TOKEN" -d "{\"name\":\"$APP_NAME\"}")
  APP_ID=$(echo "$APP" | jq -r '.id'); API_KEY=$(echo "$APP" | jq -r '.api_key')
  [ -n "$APP_ID" ] && [ "$APP_ID" != "null" ] && pass "created app ($APP_ID)" || { fail "create app: $APP"; exit 1; }

  DEPLOY=$("$BIN/zeroship" deploy "$ZSHIP" --app="$APP_ID" --control="http://localhost:$CONTROL_PORT" --token="$TOKEN" 2>&1)
  echo "$DEPLOY" | grep -q "deploy_hash" && pass "deployed real vite .zship" || { fail "deploy: $DEPLOY"; exit 1; }
else
  OUT=$("$BIN/dev-provision" --db "$DB_URL" --blob-store /tmp/gp-bundles --name "$APP_NAME" --zship "$ZSHIP")
  APP_ID=$(echo "$OUT" | awk -F= '$1 == "app_id" { print $2 }')
  API_KEY=$(echo "$OUT" | awk -F= '$1 == "api_key" { print $2 }')
  [ -n "$APP_ID" ] && [ -n "$API_KEY" ] && pass "dev-provisioned app ($APP_ID)" || { fail "dev-provision: $OUT"; exit 1; }
fi
sleep 4  # gateway route-sync poll

# --- 4. The chain works: gateway serves the deployed app ---
step 4 "Live: gateway serves the deployed app"
INDEX=$(curl -sf "http://localhost:$GATE_PORT/apps/$APP_NAME/" -H "X-Api-Key: $API_KEY" 2>/dev/null || echo "")
echo "$INDEX" | grep -qi "<!doctype html" && pass "GET / serves the app index.html" || fail "index.html not served (got: ${INDEX:0:80})"

# the hashed JS asset referenced by index.html
ASSET=$(echo "$INDEX" | grep -oE '/assets/[A-Za-z0-9._-]+\.js' | head -1)
if [ -n "$ASSET" ]; then
  code=$(curl -s -o /dev/null -w '%{http_code}' "http://localhost:$GATE_PORT/apps/$APP_NAME$ASSET" -H "X-Api-Key: $API_KEY")
  [ "$code" = "200" ] && pass "client JS asset served ($ASSET → 200)" || fail "asset $ASSET → $code"
fi

# --- 5. RPC round-trip: the deployed app's SERVER FUNCTION actually executes ---
# vite-app RPCs are at /__zeroship/v1/<wireId> (GET ?input= for queries), the
# same path the browser client uses; through the path-routed gateway that's
# /apps/<name>/__zeroship/v1/<wireId>. getMessages takes no input.
step 5 "RPC round-trip (server function executes)"
RPC=$(curl -s "http://localhost:$GATE_PORT/apps/$APP_NAME/__zeroship/v1/getMessages" \
  -H "X-Api-Key: $API_KEY" 2>/dev/null || echo "")
# Assert the SHAPE, not one substring. `grep -q "Build locally"` passed on any
# response that happened to contain that text -- an error envelope quoting the
# seed data, a truncated array, a single message, an object instead of a list.
# The dev-vs-deployed comparison below is structural, but it is RELATIVE: it
# cannot see a defect both tiers share. This is the absolute half, and it is the
# only assertion here that pins what the deployed worker actually returned.
#
# examples/starter/src/server.ts seeds exactly two messages, ids 1 and 2, each
# with a numeric createdAt. Asserting the count is what catches a partial
# result; asserting createdAt is a number is what catches the field arriving as
# a string through a serialiser change.
rpc_shape() {
  node -e '
const raw = process.argv[1];
let v;
try { v = JSON.parse(raw); } catch { console.log("not JSON"); process.exit(0); }
if (v && !Array.isArray(v) && v.json !== undefined) v = v.json;
if (!Array.isArray(v)) { console.log("not an array"); process.exit(0); }
if (v.length !== 2) { console.log(`expected 2 messages, got ${v.length}`); process.exit(0); }
const ids = v.map((m) => m && m.id).join(",");
if (ids !== "1,2") { console.log(`expected ids 1,2 got ${ids}`); process.exit(0); }
if (!v.every((m) => typeof m.text === "string" && m.text.length > 0)) {
  console.log("a message has no text"); process.exit(0);
}
if (!v.every((m) => typeof m.createdAt === "number")) {
  console.log("createdAt is not a number on every message"); process.exit(0);
}
if (!v[0].text.includes("Build locally")) { console.log("seed text missing"); process.exit(0); }
console.log("ok");
' "$1"
}
shape=$(rpc_shape "$RPC")
if [ "$shape" = "ok" ]; then
  pass "getMessages returned the 2 seeded messages, ids 1,2, with numeric createdAt"
else
  echo "  RPC response: ${RPC:0:200}"
  fail "RPC round-trip: $shape"
fi

# --- 6. The DEV half of the golden path, and it must agree with deployed ---
#
# Everything above proves the deployed side. The golden path a creator actually
# follows starts one step earlier -- `pnpm dev`, edit, refresh -- and this
# script never ran it, so scenario 1 in docs/pilot/e2e-scenarios.md read "dev
# server run repeatedly, not recorded as a scenario". Running it is half the
# point; the other half is that it must answer the SAME as the deployed app,
# because dev and deployed are different backends behind one contract and a
# divergence between them is invisible to any test that only drives one.
#
# `vite` is spawned directly rather than via `pnpm dev` so the PID is the dev
# server itself and cleanup cannot leave an orphan behind a package-manager
# wrapper.
step 6 "Dev server: pnpm dev serves the same app, and agrees with deployed"
# STARTER_API_PORT is PINNED rather than left at the 3001 default. Several
# examples share that default, so an unpinned run here inherits whatever else
# happens to be on 3001 -- and since the runtime port is not vite's, the old
# `lsof -ti :$DEV_PORT` cleanup never freed it either way.
free_ports "$DEV_PORT" "$DEV_RT_PORT"
( cd "$STARTER" && STARTER_API_PORT="$DEV_RT_PORT" ./node_modules/.bin/vite --port "$DEV_PORT" --strictPort ) >/tmp/gp-dev.log 2>&1 &
DEV_PID=$!; PIDS+=($DEV_PID)

DEV_RPC=""
DEV_RPC_URL="http://localhost:$DEV_PORT/__zeroship/v1/getMessages"
if wait_http_ok "$DEV_RPC_URL" 50; then
  DEV_RPC=$(curl -s -m 5 "$DEV_RPC_URL" 2>/dev/null || echo "")
fi

if [ -z "$DEV_RPC" ]; then
  # Say WHICH of the two failed. "never answered" was reported for both a vite
  # that never bound and a runtime that was down behind a vite serving 503s.
  if [ "$(http_status "http://localhost:$DEV_PORT/")" = "000" ]; then
    fail "vite never bound :$DEV_PORT (the dev server itself did not start)"
  else
    fail "vite is serving on :$DEV_PORT but getMessages returned $(http_status "$DEV_RPC_URL"): $(curl -s -m 5 "$DEV_RPC_URL" | head -c 200)"
  fi
  tail -20 /tmp/gp-dev.log
else
  pass "dev server executed the same server function"

  # `createdAt` is dropped from BOTH sides before comparing, and nothing else is.
  #
  # examples/starter/src/server.ts:28 seeds its messages with
  # `createdAt: Date.now() - 60_000`, evaluated when the module is first
  # imported. The dev server and the worker are separate processes that import
  # it at different moments, so the two payloads can never be byte-identical no
  # matter how correct the platform is. The first version of this check compared
  # the raw bodies and reported a divergence of 1786274106211 vs 1786274103764 --
  # a 2.4-second gap, which is process start time, not a platform defect.
  #
  # So this is normalisation of a field that is volatile BY CONSTRUCTION, not a
  # weakened assertion. What IS still compared: the envelope shape, every id and
  # text, their order, and the array length. What is NOT compared, and would be
  # missed: any divergence confined to createdAt itself.
  strip_volatile() { printf '%s' "$1" | sed -E 's/"createdAt":[0-9]+/"createdAt":<t>/g'; }
  DEV_CMP=$(strip_volatile "$DEV_RPC")
  DEP_CMP=$(strip_volatile "$RPC")

  # Set to 1 to corrupt the dev body before comparing. The comparison below is
  # the only assertion in this script that can catch a dev-vs-deployed
  # divergence, so a version of it that never fires would be worse than absent.
  [ "${MUTATE_DEV_DIVERGE:-0}" = "1" ] && DEV_CMP="${DEV_CMP}__mutated__"

  if [ "$DEV_CMP" = "$DEP_CMP" ]; then
    pass "dev and deployed agree on getMessages (identical apart from createdAt)"
  else
    fail "dev and deployed DIVERGE on the same RPC"
    echo "    dev      : ${DEV_CMP:0:160}"
    echo "    deployed : ${DEP_CMP:0:160}"
    echo "    A divergence here is the finding, not a flaky test: both sides ran the"
    echo "    same source through different backends and disagreed. createdAt is"
    echo "    already normalised out, so this is not process-start skew."
  fi
fi

# --- 6b. The request body cap, on BOTH tiers, at the byte -------------------
#
# A documented creator-facing contract with no e2e coverage on either tier
# until now. `docs/reference/runtime-limits.md` names 4 MiB
# (`MAX_REQUEST_BODY_BYTES`, enforced by the gateway); the STANDALONE server
# that `pnpm dev` runs enforces its own, smaller `MAX_BODY_BYTES` of 1 MiB
# (`crates/runtime/src/core/serve.rs`) and answers 413.
#
# The only `413` anywhere else under tests/ is e2e_platform.sh checking the
# DEPLOY endpoint against its 256 MiB COMPRESSED limit -- a different limit, on
# a different route, in a different service. It does not cover this.
#
# WHY THIS BELONGS HERE and not in a crate suite: the two tiers run different
# HTTP servers, and the only way to see that they disagree is to send the same
# bytes to both. A crate test can only ever pin one of them.
#
# THE DIVERGENCE IS THE POINT, not a bug: dev is the STRICTER tier, so it fails
# safe -- a body dev accepts is accepted deployed. The trap is the reverse
# reading, and pinning it here means a change to EITHER tier surfaces as a
# failure rather than as a creator's surprise.
#
# Boundary measured by me on the dev vector before this was written:
#   1 048 576 -> 200,  1 048 577 -> 413   (the predicate is `>`, not `>=`)
# THE URL MUST BE ONE THE GATEWAY ACTUALLY ROUTES. Written first against
# `/apps/$APP_ID/`, which answered 404 -- and a 404 is not evidence about the
# size cap, because the request never had to be read to be rejected. An
# assertion of "deployed did not answer 413" would have passed on that 404 while
# establishing nothing. The rest of this file addresses the deployed app by
# $APP_NAME (steps 5 and 6); the RPC path below is the one step 6 already
# exercises, so a body sent there is genuinely read.
BODYCAP_TMP="$(mktemp -d)"
head -c 1048577 /dev/zero | tr '\0' 'a' > "$BODYCAP_TMP/over1mib.bin"
BODYCAP_URL="http://localhost:$GATE_PORT/apps/$APP_NAME/__zeroship/v1/getMessages"

DEV_413=$(curl -s -o /dev/null -w '%{http_code}' -X POST --data-binary "@$BODYCAP_TMP/over1mib.bin" \
  -H 'Content-Type: application/octet-stream' "http://localhost:$DEV_RT_PORT/" 2>/dev/null)
DEP_1MIB=$(curl -s -o /dev/null -w '%{http_code}' -X POST --data-binary "@$BODYCAP_TMP/over1mib.bin" \
  -H 'Content-Type: application/octet-stream' -H "X-Api-Key: $API_KEY" "$BODYCAP_URL" 2>/dev/null)

# POSITIVE CONTROL for the deployed leg: the SAME url with a tiny body. Without
# it, a 404/500 from a broken route would make the size verdict below read as a
# pass, which is the trap the $APP_ID version fell into.
DEP_OK=$(curl -s -o /dev/null -w '%{http_code}' -X POST --data-binary '{"json":{}}' \
  -H 'Content-Type: application/json' -H "X-Api-Key: $API_KEY" "$BODYCAP_URL" 2>/dev/null)

echo "    body cap: dev(1MiB+1) -> $DEV_413   deployed(1MiB+1) -> $DEP_1MIB   deployed(tiny) -> $DEP_OK"

[ "$DEV_413" = "413" ] \
  && pass "dev refuses a body one byte over its 1 MiB cap (413)" \
  || fail "dev answered $DEV_413 for 1 048 577 bytes; expected 413 (serve.rs MAX_BODY_BYTES)"

[ "$DEP_OK" != "404" ] && [ "$DEP_OK" != "000" ] \
  && pass "CONTROL: the deployed body-cap URL routes (tiny body -> $DEP_OK)" \
  || fail "CONTROL: deployed body-cap URL does not route (tiny body -> $DEP_OK); the size verdict below means nothing"

# Only meaningful because the control above proves the route exists: the tiers
# disagree, and dev is the stricter one. If both answered 413 the divergence
# would be gone and this goes red.
[ "$DEP_1MIB" != "413" ] \
  && pass "deployed does NOT reject on size what dev refuses (1 MiB+1 -> $DEP_1MIB)" \
  || fail "deployed also answered 413 at 1 048 577 bytes; the documented 4 MiB cap is not in force"

# UPPER BRACKET. Without this the pair above only shows the deployed cap is
# ABOVE 1 MiB -- it could be anywhere, including absent.
#
# STATUS CODE IS THE WRONG INSTRUMENT HERE, and finding that out is what this
# block is for. There are TWO caps on the deployed path with DIFFERENT answers:
#
#   the per-resource manifest cap  -> 413 + a JSON envelope, from the
#                                     `execute_resource_tree` early-return arm
#                                     (crates/gateway/src/router/dispatch.rs)
#   the transport cap, 4 MiB       -> ntex's own response to a PayloadConfig
#                                     overflow, which gateway/src/main.rs calls
#                                     "a bare framework 400"
#
# So an over-4-MiB body and a merely-malformed body BOTH answer 400, and no
# assertion on the status alone can tell them apart. The BODY discriminates: the
# app's own rejection carries a JSON error envelope, a transport overflow does
# not. Asserted on emptiness rather than on 413, because 413 is what the OTHER
# cap produces -- an assertion expecting it here would encode a doc sentence
# (`runtime-limits.md`: "Over the cap, the caller gets 413") that describes the
# resource arm, not this one.
head -c 4194305 /dev/zero | tr '\0' 'a' > "$BODYCAP_TMP/over4mib.bin"
DEP_4MIB=$(curl -s -o /dev/null -w '%{http_code}' -X POST --data-binary "@$BODYCAP_TMP/over4mib.bin" \
  -H 'Content-Type: application/octet-stream' -H "X-Api-Key: $API_KEY" "$BODYCAP_URL" 2>/dev/null)
DEP_4MIB_BODY=$(curl -s -X POST --data-binary "@$BODYCAP_TMP/over4mib.bin" \
  -H 'Content-Type: application/octet-stream' -H "X-Api-Key: $API_KEY" "$BODYCAP_URL" 2>/dev/null | head -c 120)
DEP_1MIB_BODY=$(curl -s -X POST --data-binary "@$BODYCAP_TMP/over1mib.bin" \
  -H 'Content-Type: application/octet-stream' -H "X-Api-Key: $API_KEY" "$BODYCAP_URL" 2>/dev/null | head -c 120)

echo "    body cap: deployed(4MiB+1) -> $DEP_4MIB  body=[$DEP_4MIB_BODY]"
echo "    body cap: deployed(1MiB+1) body=[$DEP_1MIB_BODY]"

# The 4 MiB body must not be SERVED. Whatever the status, a 2xx here would mean
# the transport cap is absent and a creator can post arbitrary bytes.
case "$DEP_4MIB" in
  2??) fail "deployed ACCEPTED 4 194 305 bytes (HTTP $DEP_4MIB); the 4 MiB transport cap is not in force" ;;
  *)   pass "deployed refuses a body over its 4 MiB transport cap (HTTP $DEP_4MIB)" ;;
esac

# THE DISCRIMINATOR: the two rejections must not be the same rejection. If the
# over-cap body produced the app's own JSON envelope, the transport cap never
# fired and the app parsed 4 MiB of garbage.
[ "$DEP_4MIB_BODY" != "$DEP_1MIB_BODY" ] \
  && pass "the over-cap rejection differs from the malformed-body rejection (transport cap fired)" \
  || fail "4 MiB+1 and 1 MiB+1 produced IDENTICAL responses; the transport cap did not fire and the app parsed both"

rm -rf "$BODYCAP_TMP"

# --- 6c. The request HEADER caps on the dev tier, at the byte and at the count -
#
# The sibling of 6b, and of #42. `PayloadConfig` bounds BODIES; nothing in that
# fix bounds HEADERS, and the header side has the same inheritance shape: a
# number the code declares, and a second number it inherits from a parser
# without saying so.
#
# `serve.rs` declares `MAX_HEADER_BYTES = 16 * 1024` and checks `header_len >
# MAX_HEADER_BYTES`, so the predicate is `>` and 16384 must be ACCEPTED. That
# number was documented but never measured; both boundaries below are measured
# by me (2026-08-11) against `zeroship serve` before this step was written:
#
#     16 384 -> 200      32 headers -> 200
#     16 385 -> 431      33 headers -> 400 Bad Request
#
# THE COUNT LIMIT IS THE FINDING. `serve.rs` parses into
# `[httparse::EMPTY_HEADER; 32]`, so a 33rd header is not a size failure at all
# -- httparse returns `Err`, which takes the BAD_REQUEST arm, not the 431 arm.
# A creator who sends 33 small headers gets a bare 400 with nothing naming the
# cause, and no artifact in this repo records that 32 exists. It is not a size
# cap wearing a different code; it is a SECOND cap, undocumented.
#
# WHY /dev/tcp AND NOT curl OR nc: the assertion is about the exact byte length
# of the header block, and curl appends its own headers, so the number on the
# wire would not be the number under test. `nc` would work but is an external
# dependency this suite does not otherwise take on the golden path (only
# supabase_deploy_e2e.sh uses it), and the OpenBSD build here has no `-q`. Bash
# `/dev/tcp` is a builtin and is already the idiom in
# tests/e2e_gateway_path_backslash.sh for exactly this reason.
#
# DEV TIER ONLY, deliberately. The deployed header cap is whatever ntex defaults
# to; our code configures none, and that is an open row in
# docs/pilot/e2e-scenarios.md rather than something this step may assert.
hdr_probe() { # hdr_probe <total_header_block_bytes> -> echoes the status code
  local target="$1" fixed pad resp
  fixed=$'GET / HTTP/1.1\r\nHost: localhost:'"$DEV_RT_PORT"$'\r\nConnection: close\r\nX-Pad: \r\n\r\n'
  pad="$(head -c $(( target - ${#fixed} )) /dev/zero | tr '\0' 'x')"
  exec 3<>"/dev/tcp/127.0.0.1/$DEV_RT_PORT" || { echo "000"; return; }
  printf 'GET / HTTP/1.1\r\nHost: localhost:%s\r\nConnection: close\r\nX-Pad: %s\r\n\r\n' \
    "$DEV_RT_PORT" "$pad" >&3
  resp="$(timeout 15 cat <&3)"; exec 3<&- 2>/dev/null; exec 3>&- 2>/dev/null
  printf '%s' "$resp" | head -1 | awk '{print $2}'
}
hdr_count_probe() { # hdr_count_probe <extra_headers> -> echoes the status code
  local n="$1" hdrs="" i resp
  for i in $(seq 1 "$n"); do hdrs="${hdrs}X-h${i}: v"$'\r\n'; done
  exec 3<>"/dev/tcp/127.0.0.1/$DEV_RT_PORT" || { echo "000"; return; }
  printf 'GET / HTTP/1.1\r\nHost: localhost:%s\r\nConnection: close\r\n%s\r\n' \
    "$DEV_RT_PORT" "$hdrs" >&3
  resp="$(timeout 15 cat <&3)"; exec 3<&- 2>/dev/null; exec 3>&- 2>/dev/null
  printf '%s' "$resp" | head -1 | awk '{print $2}'
}

# THE BASELINE IS THE INSTRUMENT. The accept half must NOT assert `200`: this
# app is the starter, whose `/` is not a route, so a well-formed request answers
# 404. Asserting 200 would encode an app-ROUTING fact in a test about HEADER
# parsing -- and it did, on the first run of this step:
#
#     header cap: 16384 -> 404   16385 -> 431   32 hdrs -> 404   33 hdrs -> 400
#     ✗ dev answered 404 for 32 headers; expected 200
#
# What "accepted" means here is "the header block parsed and the request reached
# the app", and the way to say that without knowing the route is to compare
# against a TINY request to the same URL. If the baseline and the at-cap request
# answer the same thing, the header block was not the discriminator; if the
# over-cap request answers 431, the cap fired. One variable.
HDR_BASE=$(hdr_probe 200)
HDR_AT=$(hdr_probe 16384)
HDR_OVER=$(hdr_probe 16385)
HDR_N32=$(hdr_count_probe 30)   # +Host +Connection = 32 total
HDR_N33=$(hdr_count_probe 31)   # = 33 total

echo "    header cap: baseline -> $HDR_BASE   16384 -> $HDR_AT   16385 -> $HDR_OVER   32 hdrs -> $HDR_N32   33 hdrs -> $HDR_N33"

# --- the DEPLOYED half, measurement only on this pass ------------------------
# Our code configures NO header cap on the gateway; whatever ntex defaults to is
# the effective creator-facing limit, and no artifact in this repo records it.
# That is the #42 shape again (a limit inherited from a framework, differing
# from any number we declare), so it is worth a number rather than a shrug.
#
# PRINTED, NOT ASSERTED, on this pass and deliberately: I do not know the value,
# and an assertion written to match whatever the first run prints would pin a
# framework default as if it were our contract. The direction is what matters --
# dev is 16 KiB, and if deployed were SMALLER the divergence would fail UNSAFE
# (a creator's request accepted locally, refused in production). Once the number
# is in hand the next pass can assert the safe direction.
gw_hdr_probe() { # gw_hdr_probe <total_header_block_bytes> -> status
  local target="$1" path="/apps/$APP_NAME/__zeroship/v1/getMessages" fixed pad resp
  fixed=$'GET '"$path"$' HTTP/1.1\r\nHost: localhost:'"$GATE_PORT"$'\r\nConnection: close\r\nX-Pad: \r\n\r\n'
  pad="$(head -c $(( target - ${#fixed} )) /dev/zero | tr '\0' 'x')"
  exec 3<>"/dev/tcp/127.0.0.1/$GATE_PORT" || { echo "000"; return; }
  printf 'GET %s HTTP/1.1\r\nHost: localhost:%s\r\nConnection: close\r\nX-Pad: %s\r\n\r\n' \
    "$path" "$GATE_PORT" "$pad" >&3
  resp="$(timeout 15 cat <&3)"; exec 3<&- 2>/dev/null; exec 3>&- 2>/dev/null
  printf '%s' "$resp" | head -1 | awk '{print $2}'
}
GW_BASE=$(gw_hdr_probe 250)
GW_8K=$(gw_hdr_probe 8192)
GW_16K=$(gw_hdr_probe 16384)
GW_17K=$(gw_hdr_probe 16385)
GW_32K=$(gw_hdr_probe 32768)
GW_32K1=$(gw_hdr_probe 32769)
GW_64K=$(gw_hdr_probe 65536)
echo "    header cap DEPLOYED: baseline -> $GW_BASE  8192 -> $GW_8K  16384 -> $GW_16K  16385 -> $GW_17K  32768 -> $GW_32K  32769 -> $GW_32K1  65536 -> $GW_64K"

# WHAT IS ASSERTED, and what deliberately is NOT.
#
# NOT asserted: the deployed number itself. Our code configures no header cap on
# the gateway, so whatever bound exists is a FRAMEWORK DEFAULT. Pinning it here
# would record ntex's choice as if it were our contract, and the next ntex bump
# would go red as a "failure" while nothing of ours had changed. The probe line
# above prints it so a reader gets the value without a test claiming ownership.
#
# ASSERTED: the DIRECTION, which is ours. Dev caps at 16 KiB; if the deployed
# tier were ever the STRICTER one, a creator's request accepted on `pnpm dev`
# would be refused in production -- the fail-UNSAFE direction, and exactly the
# trap row 11 of docs/pilot/e2e-scenarios.md names for the body cap. 16385 is
# the byte where dev says 431, so it is the cheapest single point that settles
# the direction.
[ -n "$GW_BASE" ] && [ "$GW_BASE" != "000" ] \
  && pass "CONTROL: the gateway answers a tiny raw request ($GW_BASE)" \
  || fail "CONTROL: gateway gave no answer on a raw socket ($GW_BASE); the header verdict below means nothing"

# Compared against the BASELINE, not against a status allow-list. A list would
# have to guess which codes mean "accepted", and 404 is the trap: on this route
# it would be a perfectly good accepted-but-not-found answer, yet it reads like
# a rejection. The same mistake cost this step a red run on the dev side one day
# earlier. Same URL, same method, one variable -- the header block size.
[ "$GW_17K" = "$GW_BASE" ] \
  && pass "deployed accepts the header block dev refuses at 16 385 ($GW_17K = baseline), so dev stays the stricter tier" \
  || fail "deployed answered $GW_17K at 16 385 header bytes but $GW_BASE at 250 -- the deployed tier now bounds headers at or below dev's 16 KiB, which is the fail-UNSAFE direction: a request accepted by \`pnpm dev\` would be refused in production"

# The baseline itself must be a real answer. `000` is a failed connect, and a
# comparison against it would make every row below agree for the wrong reason.
[ -n "$HDR_BASE" ] && [ "$HDR_BASE" != "000" ] \
  && pass "CONTROL: the dev runtime answers a tiny raw request ($HDR_BASE)" \
  || fail "CONTROL: dev runtime gave no answer on a raw socket ($HDR_BASE); every header verdict below is meaningless"

[ "$HDR_AT" = "$HDR_BASE" ] \
  && pass "dev accepts a header block of exactly MAX_HEADER_BYTES (16384 -> $HDR_AT, same as baseline)" \
  || fail "dev answered $HDR_AT at 16 384 header bytes but $HDR_BASE at 200; the check is \`>\` so 16384 must be accepted"

[ "$HDR_OVER" = "431" ] \
  && pass "dev refuses one byte over the header cap (16385 -> 431)" \
  || fail "dev answered $HDR_OVER at 16 385 header bytes; expected 431 (serve.rs MAX_HEADER_BYTES)"

[ "$HDR_N32" = "$HDR_BASE" ] \
  && pass "dev accepts 32 headers, the httparse array size ($HDR_N32, same as baseline)" \
  || fail "dev answered $HDR_N32 for 32 headers but $HDR_BASE at baseline; expected them to agree"

# NOT asserted as 431: a 33rd header is a PARSE failure, not a size failure, and
# encoding it as 431 here would record a limit the server does not have.
[ "$HDR_N33" = "400" ] \
  && pass "dev refuses a 33rd header with 400, NOT 431 (a second, undocumented cap)" \
  || fail "dev answered $HDR_N33 for 33 headers; expected 400 from the httparse Err arm"

# --- 7. A dev runtime that never starts must be LOUD, not silently looping ---
#
# THE SEAM THIS TESTS, and why it cannot live in a crate suite or a vite-plugin
# unit test. The zeroship dev runtime binds a port of its OWN (devServerPort,
# default 3001) while vite binds another. `vite --port N` moves only vite's.
# Several examples take the 3001 default, so two of them running at once is not
# an exotic setup -- it is the ordinary consequence of opening a second example.
# The second runtime then cannot bind, exits in milliseconds, and the dev server
# re-spawns it. Both halves are individually correct: the runtime is right to
# refuse a taken port, and re-spawning a crashed runtime is a real feature. The
# defect is only visible where they meet, WITH VITE STILL SERVING, which is a
# two-process, two-port condition no single-process test can construct.
#
# What made it expensive: the app presents as UP. Vite answers 200 on / the
# whole time. Measured at HEAD before the fix, with examples/error-probe holding
# the runtime port, examples/starter's own `getMessages` came back
#   404 {"message":"Method not found: getMessages","code":"NOT_FOUND"}
# -- error-probe's runtime answering about a procedure it has never had. The
# proxy connects successfully, because SOMEBODY is on that port; it is just not
# your app. Two instances of the same example are worse: HTTP 200 carrying the
# other instance's data, indistinguishable from working. So this step asserts
# the runtime's failure is REFUSED at the proxy, not merely logged.
step 7 "Dev-runtime supervisor: never-starts is terminal + visible at request time"
# Step 6's dev server MUST be gone first, and the reason is a second collision
# that is not the one under test: two dev servers rooted in the SAME project
# directory also contend for `.zeroship/kv.redb`, and the loser dies with
# "Database already open. Cannot acquire lock." The first run of this step left
# step 6 running, so 7a -- the control -- failed on the redb lock rather than
# coming up, and every assertion after it was measuring a harness fault. The
# one variable between 7a and 7b has to be the PORT, so everything else that two
# dev servers can fight over is removed here.
kill "$DEV_PID" 2>/dev/null || true
free_ports "$DEV_PORT" "$DEV_RT_PORT" "$SUP_RT" "$SUP_V1" "$SUP_V2"
sleep 2
rm -f /tmp/gp-sup-a.log /tmp/gp-sup-b.log

# 7a. CONTROL. Identical to 7b in every respect except the one variable that
# matters: this one gets the runtime port to itself. Without it, a 7b that went
# red because the harness mis-starts dev servers would be indistinguishable from
# a 7b that went red because the supervisor is broken.
( cd "$STARTER" && STARTER_API_PORT="$SUP_RT" ./node_modules/.bin/vite --port "$SUP_V1" --strictPort ) >/tmp/gp-sup-a.log 2>&1 &
PIDS+=($!)
SUP_A_RPC="http://localhost:$SUP_V1/__zeroship/v1/getMessages"
if wait_http_ok "$SUP_A_RPC" 50; then
  pass "7a control: uncontended dev runtime on :$SUP_RT answers 2xx"
else
  fail "7a control: uncontended dev runtime never answered (status $(http_status "$SUP_A_RPC")) - 7b below cannot be trusted"
  tail -20 /tmp/gp-sup-a.log
fi

# 7b. THE COLLISION. Same example, same runtime port, different vite port.
( cd "$STARTER" && STARTER_API_PORT="$SUP_RT" ./node_modules/.bin/vite --port "$SUP_V2" --strictPort ) >/tmp/gp-sup-b.log 2>&1 &
PIDS+=($!)

# Vite must be up -- that is the premise of the whole failure ("the app presents
# as UP"), and asserting it separately is what keeps a red below from being read
# as "the second dev server never started".
if wait_listening "http://localhost:$SUP_V2/" 40; then
  pass "7b: the colliding dev server's vite IS serving HTTP (status $(http_status "http://localhost:$SUP_V2/"))"
else
  fail "7b: second dev server never bound :$SUP_V2"
  tail -20 /tmp/gp-sup-b.log
fi

# Terminal within a bounded time. Backoff is 1s+2s+4s over 4 attempts, so ~15s
# is the expected verdict; 60s is slack for a loaded machine, not a moving goal.
SUP_FATAL=0
for _ in $(seq 1 60); do
  grep -q "DEV RUNTIME FAILED TO START" /tmp/gp-sup-b.log && { SUP_FATAL=1; break; }
  sleep 1
done
if [ "$SUP_FATAL" = "1" ]; then
  pass "7b: supervisor gave up and printed the terminal banner"
else
  fail "7b: no terminal banner after 60s - the runtime is looping silently"
  echo "    restart lines so far: $(grep -c "runtime exited" /tmp/gp-sup-b.log)"
  tail -8 /tmp/gp-sup-b.log
fi

# Bounded, and STAYS bounded. Counting once proves nothing: the unbounded loop
# also has a finite count at any instant. The second reading 8s later is what
# separates "gave up" from "still going".
SUP_N1=$(grep -c "runtime exited" /tmp/gp-sup-b.log)
sleep 8
SUP_N2=$(grep -c "runtime exited" /tmp/gp-sup-b.log)
if [ "$SUP_N1" = "$SUP_N2" ] && [ "$SUP_N2" -le 4 ]; then
  pass "7b: restarts bounded at $SUP_N2 and stopped (no growth over 8s)"
else
  fail "7b: restart count $SUP_N1 -> $SUP_N2 (expected equal and <= 4) - still looping"
fi

# The half that actually reaches a creator. A log line they have scrolled past
# is not a signal; the response to the request they just made is.
SUP_CODE=$(http_status "http://localhost:$SUP_V2/__zeroship/v1/getMessages")
SUP_BODY=$(curl -s -m 5 "http://localhost:$SUP_V2/__zeroship/v1/getMessages" 2>/dev/null)
if [ "$SUP_CODE" = "503" ] && printf '%s' "$SUP_BODY" | grep -q "failed to start" \
   && printf '%s' "$SUP_BODY" | grep -q "already in use"; then
  pass "7b: server function returns 503 naming the real cause (port in use), not another app's answer"
else
  fail "7b: server function returned $SUP_CODE, body: ${SUP_BODY:0:200}"
  echo "    Expected 503 whose body names the port conflict. A 2xx here, or a 4xx"
  echo "    about a procedure this app DOES export, means the request was answered"
  echo "    by whoever else holds :$SUP_RT."
fi

# 7c. THE FEATURE THAT MUST NOT REGRESS. A runtime that came up, served, and
# then died is a DIFFERENT event from one that never started, and it must still
# be restarted. 7a's runtime has been up well past the healthy threshold; kill
# it with SIGABRT (SIGTERM/SIGKILL are the supervisor's own teardown signals and
# are ignored on purpose) and it must come back.
#
# WHAT THIS DOES NOT CATCH: a runtime killed by the OOM killer arrives as
# SIGKILL and is therefore NOT restarted. That is pre-existing behaviour, shared
# with the code before this step existed, and no assertion here would notice it.
# `-sTCP:LISTEN` again, and here it is not a tidiness point: 7a's VITE process
# holds a client connection to this port, so a bare `lsof -ti :$SUP_RT` can
# return the dev server itself and this would SIGABRT the parent rather than the
# runtime -- testing teardown while claiming to test crash recovery.
SUP_A_CHILD=$(lsof -ti :"$SUP_RT" -sTCP:LISTEN 2>/dev/null | head -1)
if [ -z "$SUP_A_CHILD" ]; then
  fail "7c: could not find the healthy runtime listening on :$SUP_RT"
else
  kill -ABRT "$SUP_A_CHILD" 2>/dev/null || true
  if wait_http_ok "$SUP_A_RPC" 45; then
    # A 2xx alone does NOT prove a restart happened. If the kill silently did
    # nothing, the ORIGINAL runtime is still serving and the very first poll
    # succeeds -- a pass that measures only that the harness failed to kill
    # anything. The pid having changed is what makes it an assertion.
    SUP_A_CHILD2=$(lsof -ti :"$SUP_RT" -sTCP:LISTEN 2>/dev/null | head -1)
    if [ -n "$SUP_A_CHILD2" ] && [ "$SUP_A_CHILD2" != "$SUP_A_CHILD" ]; then
      pass "7c: runtime respawned as a NEW process ($SUP_A_CHILD -> $SUP_A_CHILD2) and answers again"
    else
      fail "7c: :$SUP_RT still served by pid ${SUP_A_CHILD2:-none} (was $SUP_A_CHILD) - the kill did not land, so nothing was tested"
    fi
  else
    fail "7c: mid-session crash was not recovered (status $(http_status "$SUP_A_RPC")) - the legitimate restart path regressed"
    tail -15 /tmp/gp-sup-a.log
  fi
  if grep -q "DEV RUNTIME FAILED TO START" /tmp/gp-sup-a.log; then
    fail "7c: a single mid-session crash was treated as a never-started failure"
  else
    pass "7c: one mid-session crash did not count toward the give-up budget"
  fi
fi

# 7d. THE ORPHAN. Kill the dev server BY PID and its runtime must go with it.
#
# THE SEAM, and why neither side's own suite can see it. `zeroship serve` is a
# child of vite, and a vite killed by its recorded pid runs no teardown code at
# all -- so the runtime survives, keeps its socket, and keeps an EXCLUSIVE redb
# lock on this example's `.zeroship/kv.redb`. Measured on 2026-08-10 (task
# #221): four such survivors on ports 3151/3161/3171/3181, aged 10 to 34
# minutes, after which no dev server for this example could boot on ANY port,
# because the contended resource is the state dir and not the port.
#
# The fix is a kernel guard the CHILD arms (PR_SET_PDEATHSIG, gated on
# ZEROSHIP_DIE_WITH_PARENT -- crates/cli/src/parent_death.rs), and it only works
# if the vite plugin actually sets that variable, spelled identically, on the
# child it spawns. Those are two repos' worth of suites that cannot see each
# other: the Rust test proves the kernel behaviour with a hand-written env var,
# and the vite-plugin test proves the variable is set to a node stub that has
# never heard of prctl. ONLY A REAL DEV SERVER JOINS THEM, which is this.
#
# 7a's dev server is the subject: it has been up since 7a, and 7c has just
# proved its runtime is healthy and respawning. Nothing after this step needs
# it.
#
# BY LISTENER, not by $!. `( cd x && vite )&` records the SUBSHELL, and killing
# a subshell is not killing vite -- the assertion below would then be measuring
# a kill that never landed on the process it names.
SUP_A_VITE=$(lsof -ti :"$SUP_V1" -sTCP:LISTEN 2>/dev/null | head -1)
SUP_A_RT2=$(lsof -ti :"$SUP_RT" -sTCP:LISTEN 2>/dev/null | head -1)
# `comm` is what makes "the pid vanished" mean "the runtime vanished": a pid
# checked by existence alone reads as ALIVE the moment the kernel reuses it, so
# the after-check below would go red for a reason that has nothing to do with
# this. Read here as the positive control, and again after the kill.
gp_is_runtime() { [ -n "${1:-}" ] && [ "$(cat "/proc/$1/comm" 2>/dev/null)" = "zeroship" ]; }
if [ -z "$SUP_A_VITE" ] || ! gp_is_runtime "$SUP_A_RT2"; then
  fail "7d: could not identify the pair (vite=${SUP_A_VITE:-none} on :$SUP_V1, runtime=${SUP_A_RT2:-none} on :$SUP_RT) - nothing below was tested"
else
  pass "7d control: vite $SUP_A_VITE and its runtime $SUP_A_RT2 are two live processes before the kill"
  # SIGKILL specifically. SIGTERM would let the supervisor's own killChild run,
  # which is the path that ALREADY works and is not the one that stranded four
  # processes.
  kill -9 "$SUP_A_VITE" 2>/dev/null || true
  SUP_ORPHAN=1
  for _ in $(seq 1 20); do
    gp_is_runtime "$SUP_A_RT2" || { SUP_ORPHAN=0; break; }
    sleep 1
  done
  if [ "$SUP_ORPHAN" = "0" ]; then
    pass "7d: the runtime ($SUP_A_RT2) died with the vite that spawned it"
  else
    fail "7d: runtime $SUP_A_RT2 SURVIVED vite $SUP_A_VITE for 20s - this is task #221"
    echo "    It still holds $STARTER/.zeroship/kv.redb, so the next \`pnpm dev\` in that"
    echo "    directory cannot boot on ANY port. Check that the vite plugin sets"
    echo "    ZEROSHIP_DIE_WITH_PARENT (sdks/vite-plugin/src/dev-server.ts) and that the"
    echo "    zeroship binary on PATH is new enough to arm the guard."
    echo "    holders: $(lsof -t "$STARTER/.zeroship/kv.redb" 2>/dev/null | tr '\n' ' ')"
  fi
  # The symptom a creator actually reports is not a stray pid, it is that the
  # next dev server will not start. That is the LOCK, so assert on the lock
  # directly rather than inferring it from the pid. (Cheaper than booting a
  # second dev server, and it fails for the same reason.)
  KV_HOLDERS=$(lsof -t "$STARTER/.zeroship/kv.redb" 2>/dev/null | tr '\n' ' ')
  if [ -z "$KV_HOLDERS" ]; then
    pass "7d: nothing holds $APP_NAME's kv.redb any more - the next dev server can boot"
  else
    fail "7d: kv.redb is still held by pid(s) $KV_HOLDERS after the dev server was killed"
  fi
fi

# --- 8. The same failure on the DEPLOYED tier, measured rather than assumed ---
#
# Step 6 diffs dev vs deployed on the HAPPY path. This is the same diff on the
# failure path, and the answer is a divergence BY DESIGN rather than a defect:
# there is no supervisor on the deployed side at all, so "the app's runtime is
# unavailable" is answered by the gateway, not by a dev-server middleware.
# Recording what it actually says is the point -- the alternative is assuming
# it is fine, which is what left the dev side opaque for as long as it was.
#
# Runs LAST because it stops the worker.
step 8 "Deployed tier: the same RPC when the app's runtime is unavailable"
DEP_URL="http://localhost:$GATE_PORT/apps/$APP_NAME/__zeroship/v1/getMessages"
DEP_OK_CODE=$(curl -s -o /dev/null -w '%{http_code}' -m 5 "$DEP_URL" -H "X-Api-Key: $API_KEY")
# Kill the worker BY PID. Freeing the port by listener is the safer idiom for
# dev ports, but here the gateway is a CLIENT of this port and an over-broad
# match takes it down too -- which replaces the platform's answer with a
# connection failure and makes this step measure nothing.
kill -9 "$WORKER_PID" 2>/dev/null || true
sleep 3
DEP_DOWN_CODE=$(curl -s -o /dev/null -w '%{http_code}' -m 10 "$DEP_URL" -H "X-Api-Key: $API_KEY")
DEP_DOWN_BODY=$(curl -s -m 10 "$DEP_URL" -H "X-Api-Key: $API_KEY" 2>/dev/null)
echo "    deployed, worker up   : HTTP $DEP_OK_CODE"
echo "    deployed, worker down : HTTP $DEP_DOWN_CODE  body: ${DEP_DOWN_BODY:0:160}"
echo "    dev, runtime down     : HTTP $SUP_CODE  body: ${SUP_BODY:0:160}"
case "$DEP_DOWN_CODE" in
  5??) pass "deployed tier fails CLOSED (HTTP $DEP_DOWN_CODE) when the runtime is unavailable" ;;
  2??) fail "deployed tier answered 2xx (HTTP $DEP_DOWN_CODE) with no worker - it served something stale" ;;
  *)   fail "deployed tier returned HTTP $DEP_DOWN_CODE with no worker; expected a 5xx" ;;
esac
# The two tiers need not use the same status text -- one is a gateway, the other
# a dev proxy -- but they must AGREE that the request did not succeed. A tier
# that returns 2xx while its backend is gone is the failure this whole ticket is
# about, just moved one layer out.
if [ "${SUP_CODE:0:1}" = "5" ] && [ "${DEP_DOWN_CODE:0:1}" = "5" ]; then
  pass "dev and deployed AGREE: runtime unavailable is a 5xx on both tiers"
else
  fail "dev($SUP_CODE) and deployed($DEP_DOWN_CODE) DIVERGE on runtime-unavailable"
fi

# PUT IT BACK. This step's method is to break the stack, and a step that breaks
# the stack owes the ones after it a repair -- otherwise every later deployed
# assertion is measuring this step's leftovers. Restarting through
# gp_start_worker (rather than a second copy of the command line) is what keeps
# the restored worker identical to the original: a repair that quietly dropped
# --db would leave step 10 reporting a data-plane failure that this step caused.
if gp_start_worker; then
  pass "the worker is back up for the steps that follow (a broken stack is not left behind)"
else
  fail "the worker did not come back after step 8; every deployed assertion below is
    measuring this step's leftovers rather than the platform"
  tail -20 /tmp/gp-worker.log
fi
sleep 3   # let the gateway notice it again

# --- 9. The data plane: a migration's own field names must reach the DB ---
#
# THE SEAM THIS TESTS. Everything above drives RPC and assets; no step touches
# `env.db`, because examples/starter declares no migrations at all. That left
# the longest chain in the platform uncovered end to end:
#
#   committed migration  ->  engine DDL      (the column, spelled verbatim)
#                        ->  descriptor      (schema.runtime.json)
#                        ->  generated env.db.ts
#                        ->  installSchema   (JS field -> wire column)
#                        ->  the actual SQL
#
# Each link reads correctly alone. The defect was in the JOIN: installSchema
# snake_cased the field on the way out while every other link passed the
# authored name through, so an app whose migration declared `userId` got a
# table with a `userId` column and a data plane asking for `user_id`:
#
#     db: table default.todos has no column named user_id
#
# No crate suite can see that -- the two halves live in a Rust plugin and a JS
# SDK, and each is self-consistent. It stayed invisible for a second reason
# worth recording: all 40+ platform migrations author snake_case, on which the
# mapping is the identity, so the only fixture that could ever expose it is one
# with a case boundary in a field name. db-hitcounter's single column is
# `path`. examples/db-todos is the first.
#
# The second assertion covers the same seam for relation metadata: a migration
# declares its FK as `t.text().references("users","id")` (the vendored engine
# DSL has no t.ref()), and `with:` used to reject exactly that shape.
#
# This drives the DEV tier only, deliberately. Step 3 above deploys a .zship but
# never runs `migrated`, so the deployed stack in this script has no creator
# schema to query -- asserting against it would test the absence of a migration,
# not the presence of a column. Extending step 3 to apply creator migrations is
# what would let this become a dev-vs-deployed comparison.
step 9 "Data plane: migration field names survive to the database"
TODOS="$ROOT/examples/db-todos"

# Migrate BEFORE the dev server, because since ee2c352aa `pnpm dev` no longer
# does it. That commit split the dev-database apply out of dev-server.ts into
# its own `zeroship-dev-migrate` step, deliberately -- starting a server and
# writing a schema have different blast radii. It updated no harness, so this
# script kept passing on a `.zeroship/zs-default.sqlite` left over from BEFORE
# the split, and would have gone red the first time it ran anywhere fresh --
# which is every CI run.
#
# Measured 2026-08-10, one variable (state present vs absent, same commit):
#   with a pre-existing .zeroship   24 passed, 0 failed   exit 0
#   with .zeroship moved aside      21 passed, 1 failed   exit 1
#     ✗ users.seed did not return an id: {"message":"internal error",...}
# Note 21+1 = 22, not 24: the two checks after the seed never ran at all. That
# is what GOLDEN_MIN_PASSED exists for -- the floor caught the shortfall the
# PASS/FAIL tally alone would have under-reported.
#
# Invoked through `node <dist>` rather than `pnpm migrate` on purpose: the bin
# is declared in sdks/vite-plugin/package.json but only symlinked by an install
# that post-dates ee2c352aa, so a developer with an older node_modules gets
# `zeroship-dev-migrate: command not found` (I did). The dist path works either
# way, and this harness must not depend on when someone last installed.
MIGRATE_CLI="$ROOT/sdks/vite-plugin/dist/cli/migrate-dev.js"
if [ ! -f "$MIGRATE_CLI" ]; then
  fail "dev-migrate CLI missing at $MIGRATE_CLI (run pnpm build)"
else
  if ( cd "$TODOS" && node "$MIGRATE_CLI" ) >/tmp/gp-dbmigrate.log 2>&1; then
    pass "dev migrations applied ahead of the runtime ($(grep -o 'applied=[0-9]* skipped=[0-9]*' /tmp/gp-dbmigrate.log | tail -1))"
  else
    fail "zeroship-dev-migrate failed: $(tail -3 /tmp/gp-dbmigrate.log | tr '\n' ' ')"
  fi
fi

# The step above is a step a CREATOR must now run too, so the artifact we hand
# them has to be able to run it. `npm create zeroship-app` copies
# sdks/create-zeroship-app/template/, and ee2c352aa added the `migrate` script
# to examples/db-todos/package.json ONLY -- so a scaffolded app with a
# migrations/ directory had no command that could apply it.
#
# Asserted on the TEMPLATE rather than on a scaffolded copy on purpose: the
# template is a directory inside a workspace member, not a package, and must
# never get a node_modules (it is copied verbatim to creators -- d6ab0a80a).
# So it cannot be built or run here; its package.json is the only surface this
# harness can check, and it is the one that carries the defect.
#
# This does NOT establish that `pnpm migrate` succeeds in a scaffolded app --
# only that the command exists to be run. tests/external_chain.sh is the one
# that scaffolds for real, and it stops at `test -f dist/app.zship` without
# deploying or invoking anything (see docs/pilot/e2e-scenarios.md, scenario 1).
TEMPLATE_PKG="$ROOT/sdks/create-zeroship-app/template/package.json"
if node -e '
    const p = require(process.argv[1]);
    process.exit((p.scripts || {}).migrate ? 0 : 1);
  ' "$TEMPLATE_PKG" 2>/dev/null; then
  pass "the scaffold template declares a migrate script (creators can apply their migrations)"
else
  fail "scaffold template has no \`migrate\` script; a creator with migrations/ cannot apply them. scripts: $(node -e 'console.log(Object.keys(require(process.argv[1]).scripts||{}).join(","))' "$TEMPLATE_PKG" 2>/dev/null)"
fi

# ...and the command that script names has to be PROVIDED by something the
# template depends on. The check above is a narrow projection: it asks whether a
# `migrate` key exists, not whether running it would find a binary. Rename the
# bin in @zeroship/vite-plugin and that check stays green while every scaffolded
# creator's `npm run migrate` dies with "command not found" -- measured, not
# assumed: under exactly that mutation the existence check above returned 0.
#
# So this one asserts the BINDING, and fails differently: the first token of the
# migrate script must appear in the `bin` map of some workspace package, and
# that package must be in the template's declared dependencies. Both arms proven
# red by mutation (rename the bin -> "no workspace package provides bin"; drop
# the dep -> "comes from X, which the template does not depend on").
#
# It is still STATIC. It does not establish that the bin runs, only that the
# name a creator is told to type resolves to something we ship. The run itself
# belongs in tests/external_chain.sh, which installs outside the monorepo where
# workspace linking cannot paper over a broken package.
if node -e '
    const { readFileSync, readdirSync, existsSync } = require("fs");
    const { join } = require("path");
    const root = process.argv[1];
    const tpl = JSON.parse(readFileSync(join(root, "sdks/create-zeroship-app/template/package.json"), "utf8"));
    const script = (tpl.scripts || {}).migrate;
    if (!script) { console.error("no migrate script"); process.exit(1); }
    const cmd = script.trim().split(/\s+/)[0];
    const declared = new Set([
      ...Object.keys(tpl.dependencies || {}),
      ...Object.keys(tpl.devDependencies || {}),
    ]);
    const sdks = join(root, "sdks");
    let provider = null;
    for (const d of readdirSync(sdks)) {
      const pj = join(sdks, d, "package.json");
      if (!existsSync(pj)) continue;
      const p = JSON.parse(readFileSync(pj, "utf8"));
      const bins = typeof p.bin === "string"
        ? { [p.name.split("/").pop()]: p.bin }
        : (p.bin || {});
      if (Object.prototype.hasOwnProperty.call(bins, cmd)) { provider = p.name; break; }
    }
    if (!provider) { console.error(`no workspace package provides bin "${cmd}"`); process.exit(1); }
    if (!declared.has(provider)) {
      console.error(`bin "${cmd}" comes from ${provider}, which the template does not depend on`);
      process.exit(1);
    }
  ' "$ROOT" 2>/tmp/gp-binbind.log; then
  pass "the template's migrate command resolves to a bin we ship and declare"
else
  fail "the scaffold template's migrate command does not resolve: $(cat /tmp/gp-binbind.log)"
fi

# --- The dev server must tell a creator's mistake from a platform fault ----
#
# `regenTypesDev` (sdks/vite-plugin/src/dev-server.ts) carries a catch written
# for ONE fault: "a malformed migration must not take down the server". Right
# for that fault. But since zero-migrate's bc4d1c9b put catch_unwind on all 14
# napi exports, an ENGINE PANIC arrives at the same arm instead of killing the
# process -- and so do a missing native addon, an unparseable descriptor, and
# an unwritable generated/ dir. The creator got ONE console.error line and a
# dev server that kept serving whatever descriptor was already on disk. Every
# type error for the rest of that session is then a lie.
#
# `code` cannot separate the two classes; measured 2026-08-11 across four:
#   engine rejects the migration   Error   code undefined      <- creator's
#   migration .ts fails to build   Error   code undefined      <- creator's
#   generated/ dir unwritable      Error   code "EACCES"       <- platform
#   engine panic (upstream probe)  Error   code "GenericFailure"  platform
# and by reading, addonLoadError + the missing-runtimeJson arm + the descriptor
# parse ALSO throw code-less Errors. So the tag is CARRIED, not derived:
# gen-types throws `MigrationSourceError` for the creator's two, plain Errors
# for everything else, and the dev catch keeps its quiet arm for the tag only.
#
# ARMED WITHOUT A STUB: make the two generated artifacts unwritable. That is a
# real platform-class fault on a real dev boot, not an injected double.
#
# The mode goes on the FILES, not just the directory, and that is not a detail.
# The first version of this leg chmod'd only `generated/zeroship` to 0500 and
# read the run as a red proof. It was not: `writeFile` over an EXISTING file
# needs write permission on the FILE, and both artifacts are committed, so the
# write succeeded and the log said "regenerated". The arming had not applied,
# and a run where the fault never fired is indistinguishable from a run where
# the code failed to classify it -- both show "did not exit". The directory
# mode is kept anyway so a run that DELETES the artifacts first still arms.
#
# Both arms differ in EXACTLY one variable (the directory mode), and the
# control is what makes the fault arm mean anything: without it, "vite exited"
# would be equally explained by the harness misconfiguring the app.
step 9b "Dev server: a platform fault refuses to start; a creator's own migration does not"
PF_GEN_DIR="$TODOS/generated/zeroship"

# CONTROL arm: writable dir, same command. Waits for the regeneration to be
# REPORTED, not for a fixed delay -- otherwise a boot that never reached
# gen-types at all would pass this arm by being slow.
free_ports "$PF_V" "$PF_RT"
( cd "$TODOS" && DB_TODOS_API_PORT="$PF_RT" ./node_modules/.bin/vite --port "$PF_V" --strictPort ) \
  >/tmp/gp-faultctl.log 2>&1 &
PF_CTL_PID=$!; PIDS+=($PF_CTL_PID)
PF_CTL_OK=0
for _ in $(seq 1 40); do
  grep -q "gen-types: regenerated" /tmp/gp-faultctl.log 2>/dev/null && { PF_CTL_OK=1; break; }
  kill -0 "$PF_CTL_PID" 2>/dev/null || break
  sleep 1
done
if [ "$PF_CTL_OK" = "1" ] && kill -0 "$PF_CTL_PID" 2>/dev/null; then
  pass "control: with a writable generated/ dir, gen-types regenerates and the dev server stays up"
else
  fail "control: the db-todos dev server did not reach a successful gen-types on :$PF_V (alive=$(kill -0 "$PF_CTL_PID" 2>/dev/null && echo yes || echo no)); the fault arm below cannot be read: $(tail -3 /tmp/gp-faultctl.log | tr '\n' ' ')"
fi
kill "$PF_CTL_PID" 2>/dev/null || true
free_ports "$PF_V" "$PF_RT"

# FAULT arm: identical, with the one variable flipped.
chmod 0444 "$PF_GEN_DIR"/env.db.ts "$PF_GEN_DIR"/schema.runtime.json 2>/dev/null || true
chmod 0500 "$PF_GEN_DIR"
( cd "$TODOS" && DB_TODOS_API_PORT="$PF_RT" ./node_modules/.bin/vite --port "$PF_V" --strictPort ) \
  >/tmp/gp-faultarm.log 2>&1 &
PF_PID=$!; PIDS+=($PF_PID)
PF_EXITED=0
for _ in $(seq 1 40); do
  kill -0 "$PF_PID" 2>/dev/null || { PF_EXITED=1; break; }
  sleep 1
done
kill "$PF_PID" 2>/dev/null || true
chmod u+w "$PF_GEN_DIR" "$PF_GEN_DIR"/env.db.ts "$PF_GEN_DIR"/schema.runtime.json
free_ports "$PF_V" "$PF_RT"

if [ "$PF_EXITED" = "1" ]; then
  pass "a platform fault (unwritable generated/) stops the dev server instead of degrading it silently"
else
  fail "the dev server survived a PLATFORM fault and kept serving a stale descriptor: $(grep -c 'gen-types failed' /tmp/gp-faultarm.log) swallow line(s), still running after 40s. $(grep 'gen-types' /tmp/gp-faultarm.log | head -2 | tr '\n' ' ')"
fi

# The exit alone does not prove the fault was CLASSIFIED -- a crash for any
# other reason exits too. This arm reads the classification the creator sees.
if grep -q "PLATFORM FAULT" /tmp/gp-faultarm.log 2>/dev/null; then
  pass "the failure names itself a platform fault, not the creator's migrations"
else
  fail "the dev server did not classify the fault; a creator sees only: $(grep -i 'gen-types' /tmp/gp-faultarm.log | head -2 | tr '\n' ' ')"
fi

free_ports "$DB_V" "$DB_RT"
( cd "$TODOS" && DB_TODOS_API_PORT="$DB_RT" ./node_modules/.bin/vite --port "$DB_V" --strictPort ) \
  >/tmp/gp-dbtodos.log 2>&1 &
PIDS+=($!)

DB_RPC="http://localhost:$DB_RT/__zeroship/v1"
db_call() { curl -sS -m 10 -X POST -H 'content-type: application/json' "$DB_RPC/$1" -d "{\"json\":$2}"; }

# The wire ids used here are the PUBLISHED ids (`users.seed`, `todos.create`),
# which are NOT the export names (`seedUser`, `createTodo`) -- see the note at
# step 1b. A call against an export name answers `Method not found` forever.
#
# Readiness and the seed are SEPARATE steps on purpose. Folding them together
# (retrying `users.seed` until it returns an id) looks tidier and is wrong: it
# retries through real errors as though they were "not up yet", so a genuine
# defect is reported as "the runtime never came up" with the actual cause
# buried. Observed while building this: the loop reported a startup timeout
# when what had happened was `UNIQUE constraint failed: users.handle`.
#
# `users.handle` and `users.email` are UNIQUE, so a fixed literal makes this
# leg pass exactly once per database and fail on every re-run. The identity is
# per-run.
GP_TAG="gp-$$-${RANDOM}"
# MADE/JOINED are assigned ONLY inside the `DB_UP -eq 1` branch below, but the
# dev-vs-deployed divergence messages interpolate them unguarded. Under
# `set -uo pipefail` that is a HARD ABORT, not a `fail()` - the harness dies
# mid-run and every later step is lost, so a real dev-tier outage reports as a
# crashed script rather than a red row.
#
# MEASURED, not hypothetical: a 20-run campaign hit this in runs 8-12, five
# CONSECUTIVE runs, byte-identical from step 9 onward, with runs 1-7 and 13-20
# clean. `tests/golden_path.sh: line 1180: MADE: unbound variable`. The trigger
# was the dev leg never binding (redb lock held by an untracked process), which
# leaves DEV_INSERT_VERDICT at "not-run" while the deployed leg answers "ok" -
# so the verdicts differ, the divergence arm fires, and it reaches for a variable
# that was never declared.
#
# Defaulting them here keeps the diagnostic honest: the row goes RED and says the
# dev leg did not run, which is the truth, instead of taking the script down.
MADE=""
JOINED=""
DEV_INSERT_VERDICT="not-run"
DEV_JOIN_VERDICT="not-run"
DB_UP=0
for _ in $(seq 1 60); do
  if [ "$(http_status "http://localhost:$DB_RT/__zeroship/v1/todos.list")" != "000" ]; then
    DB_UP=1; break
  fi
  sleep 1
done

if [ "$DB_UP" -ne 1 ]; then
  fail "db-todos dev runtime never bound :$DB_RT (data-plane leg could not run)"
  tail -20 /tmp/gp-dbtodos.log
else
  SEED=$(db_call users.seed "{\"email\":\"$GP_TAG@example.com\",\"name\":\"GP\",\"handle\":\"$GP_TAG\"}")
  GP_ID=$(printf '%s' "$SEED" | sed -nE 's/.*"id":"([^"]+)".*/\1/p')
  if [ -z "$GP_ID" ]; then
    fail "users.seed did not return an id: ${SEED:0:200}"
  else
    pass "seeded a user through env.db"

    # The camelCase field is the whole point: `userId` must arrive as the
    # column the migration created, not a snake_cased rewrite of it.
    MADE=$(db_call todos.create "{\"userId\":\"$GP_ID\",\"title\":\"golden path\",\"priority\":\"low\"}")
    case "$MADE" in
      *'"id":'*) DEV_INSERT_VERDICT="ok"; pass "insert with a camelCase field (userId) reached the migration's column" ;;
      *) DEV_INSERT_VERDICT="fail"; fail "insert with a camelCase field FAILED: ${MADE:0:220}
    This is the descriptor-name-vs-column seam. A body naming a column that
    'has no column named ...' means a link in the chain renamed the field." ;;
    esac

    # A foreign key DECLARED IN A MIGRATION must be usable as a relation.
    JOINED=$(db_call todos.listWithUser "{\"userId\":\"$GP_ID\"}")
    # Asserting on THIS run's email, not a literal: the joined row must be the
    # user this run seeded. Matching a fixed address would pass on a row some
    # earlier run left behind.
    case "$JOINED" in
      *"$GP_TAG@example.com"*) DEV_JOIN_VERDICT="ok"; pass "with: eager-loaded across a migration-declared FK" ;;
      *) DEV_JOIN_VERDICT="fail"; fail "with: did not join across the migration's FK: ${JOINED:0:220}
    'is not a t.ref field' here means relation loading is gated on a type
    token the migration-first pipeline cannot produce." ;;
    esac
  fi
fi

# --- 9b. THE SAME ASSERTIONS, deployed, diffed against the dev results above -
#
# Row 2 of docs/pilot/e2e-scenarios.md said this in as many words: "Deployed
# half not compared - see row 11." Row 11 never closed it either - it walks
# `env.db` CRUD/relations/transactions on both tiers via db-hitcounter/db-todos
# through a SEPARATE harness (tests/e2e_dev_vs_deployed_db.sh), and its own
# note records that db-hitcounter's one column ("path") has no case boundary,
# so it is BLIND BY CONSTRUCTION to the exact naming seam this step exists to
# catch. This closes it inside golden_path.sh itself: build db-todos through
# the real vite-plugin, deploy it, apply ITS OWN migrations through
# zeroship-migrated (the path #162 lived in), drive the SAME two RPC calls
# through the gateway, and diff the RESULT against the dev run above -
# following step 10's pattern (offline-mint a platform-admin PAT,
# dev-provision, POST the recorded IR to zeroship-migrated) rather than
# inventing a new one.
#
# A SEPARATE creator/PAT and a separate app name ("dbtodos9") from step 11's
# later "dbtodos" deploy, on purpose: step 11 runs after step 10 and reuses
# step 10's scaffold PAT/creator, neither of which exists yet here (step 9
# runs first). Two independent deploys of the same source under different
# names is the same shape step 10 and step 11 already use for the scaffold
# app - it costs one extra build and provision, not new cross-step plumbing.
#
# THE FIXTURE MUST BE ABLE TO FAIL, and this is checked by RUNNING, not by
# eyeballing the migration file. If every authored column were a single
# lowercase word, `installSchema`'s snake_case-vs-asIs naming strategy would
# be a no-op on it and a dev/deployed AGREE verdict below would mean nothing -
# exactly the step-11 `created_at`-population trap this file's own history
# warns about (a non-discriminating fixture that prints agreement over a
# platform already proven broken). So before trusting anything below, parse
# the ACTUAL migration file - not a name copied into this script - and require
# at least one authored column to contain a lower-to-upper case boundary.
DB9_CASE_FIELD="$(node -e '
    const fs = require("fs");
    const src = fs.readFileSync(process.argv[1], "utf8");
    const re = /(\w+)\s*:\s*t\./g;
    let m, found = null;
    while ((m = re.exec(src))) { if (/[a-z][A-Z]/.test(m[1])) { found = m[1]; break; } }
    process.stdout.write(found || "");
  ' "$TODOS/migrations/20260101000000_create_todos.ts")"
if [ -n "$DB9_CASE_FIELD" ]; then
  DB9_SNAKE="$(node -e 'console.log(process.argv[1].replace(/([a-z0-9])([A-Z])/g,"$1_$2").toLowerCase())' "$DB9_CASE_FIELD")"
  pass "fixture can discriminate: db-todos' migration authors '$DB9_CASE_FIELD' with a case boundary (a snake_case rewrite would corrupt it to '$DB9_SNAKE')"
else
  fail "FAILED SETUP: no column in db-todos' migration contains a case boundary. A snake_case
    naming rewrite would be a no-op on every field, so the dev-vs-deployed comparison below
    could not have caught the #162-class defect however broken the platform is. Do not trust
    a green from this step until a camelCase (or otherwise case-boundary) column exists."
fi

DB9_APP="dbtodos9"
DB9_READY=0
# The build and the artifact check are SEPARATE conditions, deliberately. They
# were one `&&` chain, and the else-arm said "does not build" for both - so an
# app that built fine but left no artifact reported a build failure, sending the
# reader to the wrong half. MEASURED 2026-08-12: a run failed here with a
# ZERO-BYTE build log, which left the printed message empty after the colon and
# nothing to tell the two apart; `pnpm --filter db-todos build` by hand
# immediately afterwards succeeded, so the failure was not persistent and its
# cause is still unidentified. That is the situation this split exists to stop
# recurring: capture the exit code, say which half failed, and report the log
# SIZE even when it is empty (an empty log is itself a datum - it means the
# command produced no output at all, which a successful vite build never does).
( cd "$TODOS" && pnpm build ) >/tmp/gp-dbtodos9-build.log 2>&1
DB9_BUILD_RC=$?
DB9_BUILD_BYTES=$(wc -c </tmp/gp-dbtodos9-build.log 2>/dev/null | tr -d ' ')
if [ "$DB9_BUILD_RC" = "0" ] && [ -f "$TODOS/dist/app.zship" ]; then
  pass "db-todos builds through the real vite-plugin for the deployed leg ($(du -k "$TODOS/dist/app.zship" | cut -f1)KB)"

  DB9_OUT=$("$BIN/dev-provision" --db "$DB_URL" --blob-store /tmp/gp-bundles --name "$DB9_APP" --zship "$TODOS/dist/app.zship" 2>&1)
  DB9_APP_ID=$(echo "$DB9_OUT" | awk -F= '$1 == "app_id" { print $2 }')
  DB9_API_KEY=$(echo "$DB9_OUT" | awk -F= '$1 == "api_key" { print $2 }')
  if [ -z "$DB9_APP_ID" ] || [ -z "$DB9_API_KEY" ]; then
    fail "could not provision db-todos for the deployed leg: ${DB9_OUT:0:200}"
  else
    # Offline-mint a platform-admin PAT, following step 10's mechanism exactly
    # (see step 10 for why it must be offline: no OP runs in this script).
    DB9_POLICY='{"name":"golden-db9","statements":[{"effect":"allow","actions":["apps:read","apps:write","apps:deploy","billing:read","billing:write"],"resources":[{"type":"any"}],"conditions":[]}]}'
    DB9_PHASH="$(node -e 'const{createHash}=require("crypto");function c(v){if(v===null||typeof v==="number"||typeof v==="boolean")return JSON.stringify(v);if(typeof v==="string")return JSON.stringify(v);if(Array.isArray(v))return "["+v.map(c).join(",")+"]";return "{"+Object.keys(v).sort().map(k=>JSON.stringify(k)+":"+c(v[k])).join(",")+"}";}process.stdout.write(createHash("sha256").update(c(JSON.parse(process.argv[1]))).digest("hex"));' "$DB9_POLICY")"
    DB9_CREATOR="$(node -e 'console.log(require("crypto").randomUUID())')"
    DB9_TOKID="$(node -e 'console.log(require("crypto").randomUUID())')"
    DB9_EXP=$(( $(date +%s) + 86400 ))
    docker exec -i "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.users (id,email,name,email_verified_at) VALUES ('$DB9_CREATOR','golden-db9-$DB9_CREATOR@zeroship.test'::citext,'Golden DB9',NOW());
INSERT INTO zeroship.platform_admin_roles (user_id,role,granted_by) VALUES ('$DB9_CREATOR','admin','$DB9_CREATOR');
INSERT INTO zeroship.permission_tokens (id,owner_id,kind,name,policies,policy_hash,expires_at) VALUES ('$DB9_TOKID','$DB9_CREATOR','pat','golden db9','$DB9_POLICY'::jsonb,'$DB9_PHASH',to_timestamp($DB9_EXP));
INSERT INTO zeroship.app_members (app_id,user_id,role) VALUES ('$DB9_APP_ID','$DB9_CREATOR','owner') ON CONFLICT (app_id,user_id) DO UPDATE SET role='owner';
SQL
    DB9_JOSE="$ROOT/node_modules/.pnpm/jose@6.2.3/node_modules/jose/dist/webapi/index.js"
    DB9_PAT="$(node --input-type=module -e 'import{readFileSync}from "node:fs";import{createHash,randomBytes}from "node:crypto";import{importPKCS8,exportJWK,SignJWT}from "file://'"$DB9_JOSE"'";const[pem,owner,tid,phash,exp]=process.argv.slice(1);const key=await importPKCS8(readFileSync(pem,"utf8"),"EdDSA",{extractable:true});const x=(await exportJWK(key)).x;const kid=createHash("sha256").update(`{"crv":"Ed25519","kty":"OKP","x":"${x}"}`).digest("base64url");const jwt=await new SignJWT({sub:owner,owner,tid,jti:tid,scope:"pat",policy_hash:phash,nonce:randomBytes(32).toString("base64url")}).setProtectedHeader({alg:"EdDSA",typ:"pat+jwt",kid}).setIssuer("https://api.zeroship.ai").setAudience("control.zeroship.ai").setIssuedAt(Math.floor(Date.now()/1000)).setExpirationTime(Number(exp)).sign(key);process.stdout.write(jwt);' "$GP_SIGNING_KEY" "$DB9_CREATOR" "$DB9_TOKID" "$DB9_PHASH" "$DB9_EXP" 2>/tmp/gp-dbtodos9-pat.log)"

    node --input-type=module - "$ROOT/sdks/vite-plugin/dist/gen-types/recorder.js" "$TODOS/migrations" >/tmp/gp-dbtodos9-ir.json 2>/tmp/gp-dbtodos9-ir.log <<'NODE'
import { pathToFileURL } from "node:url";
const [recorderPath, dir] = process.argv.slice(2);
const { discoverMigrations, recordMigration } = await import(pathToFileURL(recorderPath).href);
const migrations = await discoverMigrations(dir);
const documents = [];
for (const m of migrations) documents.push({ filename: m.stem + ".ir.json", body: await recordMigration(m.path) });
console.log(JSON.stringify({ kind: "ir", documents }));
NODE
    DB9_APPLY_CODE="$(curl -s -o /tmp/gp-dbtodos9-apply.json -w '%{http_code}' -X POST \
      "http://localhost:$MIGRATED_PORT/v1/apps/$DB9_APP_ID/migrations/apply" \
      -H 'Content-Type: application/json' -H "Authorization: Bearer $DB9_PAT" \
      --data-binary @/tmp/gp-dbtodos9-ir.json)"
    DB9_APPLIED="$(node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const o=JSON.parse(s);process.stdout.write(String((o.applied||[]).length))}catch{process.stdout.write("0")}})' </tmp/gp-dbtodos9-apply.json)"
    if [ "$DB9_APPLY_CODE" = "200" ] && [ "${DB9_APPLIED:-0}" -ge 1 ] 2>/dev/null; then
      pass "db-todos' own migrations applied through zeroship-migrated to the deployed app (applied=$DB9_APPLIED ops)"
      DB9_READY=1
      sleep 6   # gateway route-sync poll
    else
      fail "zeroship-migrated could not apply db-todos' migrations for the deployed leg (http=$DB9_APPLY_CODE): $(head -c 200 /tmp/gp-dbtodos9-apply.json)"
    fi
  fi
else
  if [ "$DB9_BUILD_RC" != "0" ]; then
    fail "db-todos BUILD FAILED for the deployed leg (exit=$DB9_BUILD_RC, log=${DB9_BUILD_BYTES:-0} bytes): $(tail -5 /tmp/gp-dbtodos9-build.log | tr '\n' ' ')
      A ZERO-byte log here means the command produced no output at all, which a
      successful vite build never does - suspect the subshell being killed or
      \`cd $TODOS\` failing, NOT a compile error."
  else
    fail "db-todos BUILT (exit=0) but left no $TODOS/dist/app.zship - the artifact, not the build, is what is missing"
  fi
fi

DB9_BASE="http://localhost:$GATE_PORT/apps/$DB9_APP"
db9_call() { curl -sS -m 10 -X POST -H 'content-type: application/json' -H "X-Api-Key: $DB9_API_KEY" "$DB9_BASE/__zeroship/v1/$1" -d "{\"json\":$2}"; }

DEP_INSERT_VERDICT="not-run"
DEP_JOIN_VERDICT="not-run"
DEP_MADE=""
DEP_JOINED=""
if [ "$DB9_READY" -ne 1 ]; then
  fail "the deployed db-todos never became drivable; the deployed half of this scenario did not run"
else
  DEP_TAG="gp9-$$-${RANDOM}"
  DEP_SEED=$(db9_call users.seed "{\"email\":\"$DEP_TAG@example.com\",\"name\":\"GP9\",\"handle\":\"$DEP_TAG\"}")
  DEP_ID=$(printf '%s' "$DEP_SEED" | sed -nE 's/.*"id":"([^"]+)".*/\1/p')
  if [ -z "$DEP_ID" ]; then
    fail "deployed users.seed did not return an id: ${DEP_SEED:0:200}"
  else
    pass "deployed: seeded a user through env.db"

    DEP_MADE=$(db9_call todos.create "{\"userId\":\"$DEP_ID\",\"title\":\"golden path deployed\",\"priority\":\"low\"}")
    case "$DEP_MADE" in
      *'"id":'*) DEP_INSERT_VERDICT="ok"; pass "deployed: insert with a camelCase field (userId) reached the migration's column" ;;
      *) DEP_INSERT_VERDICT="fail"; fail "deployed: insert with a camelCase field FAILED: ${DEP_MADE:0:220}
    This is the descriptor-name-vs-column seam, deployed: a body naming a column that
    'has no column named ...' means a link in the chain renamed the field." ;;
    esac

    DEP_JOINED=$(db9_call todos.listWithUser "{\"userId\":\"$DEP_ID\"}")
    case "$DEP_JOINED" in
      *"$DEP_TAG@example.com"*) DEP_JOIN_VERDICT="ok"; pass "deployed: with: eager-loaded across a migration-declared FK" ;;
      *) DEP_JOIN_VERDICT="fail"; fail "deployed: with: did not join across the migration's FK: ${DEP_JOINED:0:220}" ;;
    esac
  fi
fi

# THE COMPARISON. Diff RESULTS, not code - the dev verdicts were captured by
# the two `case` statements above into DEV_INSERT_VERDICT / DEV_JOIN_VERDICT.
# Compare them against the deployed verdicts just captured, on the SAME
# question each time, rather than only asserting each tier individually.
if [ "$DEV_INSERT_VERDICT" = "$DEP_INSERT_VERDICT" ]; then
  pass "camelCase insert reaching the migration's column: dev and deployed AGREE ($DEV_INSERT_VERDICT)"
else
  fail "camelCase insert reaching the migration's column: dev and deployed DIVERGE -- dev=$DEV_INSERT_VERDICT deployed=$DEP_INSERT_VERDICT
    dev      : ${MADE:0:150}
    deployed : ${DEP_MADE:0:150}"
fi
if [ "$DEV_JOIN_VERDICT" = "$DEP_JOIN_VERDICT" ]; then
  pass "FK eager-load across a migration-declared relation: dev and deployed AGREE ($DEV_JOIN_VERDICT)"
else
  fail "FK eager-load across a migration-declared relation: dev and deployed DIVERGE -- dev=$DEV_JOIN_VERDICT deployed=$DEP_JOIN_VERDICT
    dev      : ${JOINED:0:150}
    deployed : ${DEP_JOINED:0:150}"
fi

# --- 10. The app a creator ACTUALLY receives, on both tiers ------------------
#
# THE SEAM THIS TESTS, and why every step above is blind to it: steps 1-9 build
# `examples/starter` and `examples/db-todos`. Neither is what `npm create
# zeroship-app` produces. That command copies
# `sdks/create-zeroship-app/template/`, and the template differs from the
# starter in the one place that decides whether a deployed app answers at all --
# the starter ships `src/server/config.ts` declaring its RPC policy, and the
# template ships no policy anywhere. A harness that only ever builds the
# configured app cannot see this by construction, which is exactly why the
# claim sat on record for a day as an INFERENCE.
#
# WHAT THIS ASSERTS, and what it deliberately does NOT. It does not assert 401,
# and it must not: whether the template SHOULD ship `auth: "anon"` (an
# anonymous write surface in every new app) or explicit `auth: "user"` (which
# still 401s, but silences the build's only warning) is an open operator
# decision, and a gate that pinned either one would be asserting an answer
# nobody has given. What it asserts is that `pnpm dev` and the deployed app
# AGREE on each procedure. Divergence is the finding, in whichever direction it
# points, and the gate stays correct after the operator decides either way.
#
# THIS IS RED AT HEAD, BY DESIGN. All six procedures answer 200 in dev and 401
# deployed. The six failures below are the defect, not a broken test -- the
# same posture as the publish-list gate. When the template gains a policy they
# turn green with no edit to this file.
#
# THE VEHICLE, and its one honest substitution. `template/` is a directory
# inside a workspace member, not a package, and must never grow a node_modules
# (it is copied verbatim to creators -- d6ab0a80a). Installing a scaffolded copy
# from a registry is blocked by task #265: the published @zeroship/vite-plugin
# requires zero-migrate@0.1.0, which exists on no registry, so `npm install`
# dies E404. So `examples/scaffold-app` is the template's real source, produced
# by running the SHIPPED `bin/create.js`, with ONE class of edit: the
# `@zeroship/*` dependency specs are `workspace:*` instead of registry semver.
# PRESERVED: every source byte, all six procedures, the ABSENT policy, the
# migrations, the generated descriptor, and the real vite-plugin build. CHANGED:
# dependency resolution only. 10a is what keeps that claim true over time.
step 10 "Scaffold: the app \`npm create zeroship-app\` produces, on both tiers"
SCAFFOLD="$ROOT/examples/scaffold-app"
TEMPLATE="$ROOT/sdks/create-zeroship-app/template"
SC_APP="scaffoldapp"
# A crashed earlier run can leave the CONTROL leg's policy file behind, which
# would silently turn this run's measurement into the control. Remove it before
# anything reads the tree, not only in cleanup.
rm -rf "$SCAFFOLD/src/server"

# --- 10a. The vehicle is the template, and stays the template ---------------
#
# Without this the whole step is worthless: `examples/scaffold-app` could drift
# into a hand-tuned app that reproduces nothing, and every green below would be
# about a file nobody ships. So the harness re-runs the REAL scaffolder into a
# temp dir and requires byte equality on every file except package.json, plus a
# structural check that package.json differs ONLY in `name` and in `@zeroship/*`
# specs.
#
# THE EXCLUDE LIST IS NOT MINE. Build and run state has to be excluded from the
# comparison, but choosing that list by hand is how a real added file gets
# quietly waved through. So it is read from the TEMPLATE'S OWN `.gitignore`:
# whatever the template tells a creator not to track is, by the template's own
# statement, not source. Anything else appearing on either side is drift and
# fails. That inversion found a defect on the first run -- `pnpm dev` writes a
# `blob-cache/` directory into the project root and the template's ignore file
# did not list it, so every scaffolded app got an untracked directory. Invisible
# inside this monorepo because the ROOT .gitignore covers it (line 12); a
# creator's repo has only the template's.
SC_FRESH="$(mktemp -d -t gp-scaffold-XXXXXX)"
if ( cd "$SC_FRESH" && node "$ROOT/sdks/create-zeroship-app/bin/create.js" scaffold-app ) >/tmp/gp-scaffold-create.log 2>&1; then
  SC_EXCL=(--exclude=package.json)
  while IFS= read -r ig; do
    case "$ig" in ""|"#"*) continue ;; esac
    SC_EXCL+=("--exclude=${ig%/}")
  done < "$SC_FRESH/scaffold-app/.gitignore"
  SC_DIFF="$(diff -r -q "$SC_FRESH/scaffold-app" "$SCAFFOLD" "${SC_EXCL[@]}" 2>&1)"
  if [ -z "$SC_DIFF" ]; then
    pass "examples/scaffold-app is byte-identical to the shipped template (source files)"
  else
    fail "the scaffold copy has DRIFTED from sdks/create-zeroship-app/template -- it no longer
    reproduces what a creator receives, so every verdict below is about a different app:
$(printf '%s' "$SC_DIFF" | sed 's/^/      /')"
  fi
  if node -e '
      const { readFileSync } = require("fs");
      const [a, b] = process.argv.slice(1).map((p) => JSON.parse(readFileSync(p, "utf8")));
      const bad = [];
      // `name` is rewritten by create.js itself; every other top-level key must match.
      for (const k of new Set([...Object.keys(a), ...Object.keys(b)])) {
        if (k === "name" || k === "dependencies" || k === "devDependencies") continue;
        if (JSON.stringify(a[k]) !== JSON.stringify(b[k])) bad.push(`top-level "${k}" differs`);
      }
      for (const sect of ["dependencies", "devDependencies"]) {
        const da = a[sect] || {}, db = b[sect] || {};
        for (const d of new Set([...Object.keys(da), ...Object.keys(db)])) {
          if (!(d in da) || !(d in db)) { bad.push(`${sect}: "${d}" present in only one`); continue; }
          if (da[d] === db[d]) continue;
          // The ONE permitted substitution, and only for first-party packages.
          if (d.startsWith("@zeroship/") && db[d] === "workspace:*") continue;
          bad.push(`${sect}: "${d}" ${JSON.stringify(da[d])} -> ${JSON.stringify(db[d])}`);
        }
      }
      if (bad.length) { console.error(bad.join("; ")); process.exit(1); }
    ' "$SC_FRESH/scaffold-app/package.json" "$SCAFFOLD/package.json" 2>/tmp/gp-scaffold-pkg.log; then
    pass "the copy's package.json differs from the template's ONLY in workspace: dep specs"
  else
    fail "the scaffold copy substitutes more than dependency resolution: $(cat /tmp/gp-scaffold-pkg.log)"
  fi
else
  fail "could not re-scaffold from bin/create.js (the fidelity check could not run): $(tail -3 /tmp/gp-scaffold-create.log | tr '\n' ' ')"
fi
rm -rf "$SC_FRESH"

# --- 10b. Build it, and read the posture out of the ARTIFACT ----------------
#
# The build warning and the manifest must say the same thing. Asserting the
# warning alone would pin today's posture; asserting the manifest alone would
# miss a build that stops warning. So: whichever rpc ids carry no `auth` in the
# built manifest are exactly the ids the build warned about. That invariant
# holds before AND after the operator decides, which is the point.
SC_ZSHIP="$SCAFFOLD/dist/app.zship"

# THE MUTATION THAT PROVES THIS STEP IS A MEASUREMENT AND NOT A HARD-CODED RED.
# Six identical failures are what a step wired to fail would also print, so the
# red below is worth nothing on its own. Setting MUTATE_SCAFFOLD_POLICY=1 gives
# the scaffold the policy the template lacks, BEFORE the build -- nothing else
# in the step changes. Under it every assertion IN THIS STEP must go GREEN,
# including the six comparisons and the control (which then flips the other way,
# removing the policy).
#
#     measured 2026-08-10    unmutated              43 passed,  6 failed
#                            MUTATE_SCAFFOLD_POLICY 49 passed,  0 failed
#     measured 2026-08-11    unmutated              67 passed,  8 failed
#                            MUTATE_SCAFFOLD_POLICY 73 passed,  2 failed
#     measured 2026-08-11 (later, after steps 2c and the grant assertions)
#                            unmutated              81 passed,  8 failed
#     measured 2026-08-11 (later still, + the pairwise-sector pair)
#                            unmutated              83 passed,  8 failed
#
# The 81 is what GOLDEN_MIN_PASSED is set to, so the floor now sits EXACTLY at
# the pass count with no slack: any assertion that stops firing takes the run
# below it. That is deliberate and it is measured, not derived - an earlier run
# the same day read 80 passed / 9 failed and I nearly lowered the floor to match
# it. The ninth failure was a leftover, gitignored examples/db-todos/.zeroship
# whose dev.sqlite journal predated a change to the migration body; with that
# directory moved aside the same command goes from a checksum-drift failure to
# `applied=5 skipped=0`. CI checks out fresh and cannot reproduce it. The eight
# that remain are the known red-by-design set: step 10's six (#260) and step
# 11's two (#255).
#
# BOTH rows are kept because the pair is the point: the whole-harness totals
# moved (43 -> 67) as steps landed, and the "0 failed" of the 2026-08-10 row
# stopped being reachable when step 11's collation reds (#255) arrived - they
# are not scaffold failures and no policy clears them. What did NOT move is the
# invariant: **the delta is exactly the six**, and the control fires in BOTH
# directions, which is what separates "the gateway honours the manifest" from
# "this harness always 401s". Read the delta, not the totals.
if [ "${MUTATE_SCAFFOLD_POLICY:-0}" = "1" ]; then
  mkdir -p "$SCAFFOLD/src/server"
  {
    echo 'import { defineApp } from "@zeroship/server";'
    echo 'export default defineApp({ resources: {'
    echo '  "rpc:notes.list": { auth: "anon", publiclyAccessible: true },'
    echo '  "rpc:notes.add": { auth: "anon", publiclyAccessible: true },'
    echo '  "rpc:notes.delete": { auth: "anon", publiclyAccessible: true },'
    echo '  "rpc:files.upload": { auth: "anon", publiclyAccessible: true },'
    echo '  "rpc:files.list": { auth: "anon", publiclyAccessible: true },'
    echo '  "rpc:visits.bump": { auth: "anon", publiclyAccessible: true },'
    echo '} });'
  } > "$SCAFFOLD/src/server/config.ts"
  echo "  MUTATION ACTIVE: scaffold given an anon policy before the build"
fi
( cd "$SCAFFOLD" && pnpm build ) >/tmp/gp-scaffold-build.log 2>&1
SC_BUILD_RC=$?
if [ "$SC_BUILD_RC" -ne 0 ] || [ ! -f "$SC_ZSHIP" ]; then
  fail "the scaffold template does not build (rc=$SC_BUILD_RC): $(tail -5 /tmp/gp-scaffold-build.log | tr '\n' ' ')"
else
  pass "the scaffold template builds through the real vite-plugin (exit 0, $(du -k "$SC_ZSHIP" | cut -f1)KB)"
fi

# ids with no `auth` key in the manifest, comma-separated and sorted
sc_unpoliced() {
  tar --zstd -xOf "$1" manifest.json 2>/dev/null | node -e '
    let s=""; process.stdin.on("data",d=>s+=d).on("end",()=>{
      let m; try { m = JSON.parse(s); } catch { process.exit(4); }
      const r = m.resources || {};
      const ids = Object.keys(r).filter(k => k.startsWith("rpc:") && r[k].auth === undefined).sort();
      process.stdout.write(ids.join(","));
    });'
}
sc_all_rpc() {
  tar --zstd -xOf "$1" manifest.json 2>/dev/null | node -e '
    let s=""; process.stdin.on("data",d=>s+=d).on("end",()=>{
      let m; try { m = JSON.parse(s); } catch { process.exit(4); }
      process.stdout.write(Object.keys(m.resources||{}).filter(k=>k.startsWith("rpc:")).sort().join(","));
    });'
}
SC_UNPOLICED="$(sc_unpoliced "$SC_ZSHIP")"
# The build's warning lists one `  - "rpc:..."` line per offender.
SC_WARNED="$(grep -oE '^  - "rpc:[^"]+"' /tmp/gp-scaffold-build.log | tr -d '" ' | sed 's/^-//' | sort | paste -sd, -)"
if [ "$SC_WARNED" = "$SC_UNPOLICED" ]; then
  if [ -n "$SC_UNPOLICED" ]; then
    pass "build warned about exactly the unpoliced ids: $SC_UNPOLICED"
  else
    pass "the template declares a policy for every procedure and the build emits no warning"
  fi
else
  fail "the build's fail-closed warning and the manifest disagree.
      manifest says unpoliced: ${SC_UNPOLICED:-<none>}
      build warned about     : ${SC_WARNED:-<none>}
    One of the two is lying about what a creator is shipping."
fi

# The six probes below are hand-written per procedure. If the template gains or
# loses one, the honest response is a new probe -- NOT a silently smaller
# comparison, which is how a scenario stops covering what its title claims.
SC_EXPECT_IDS="rpc:files.list,rpc:files.upload,rpc:notes.add,rpc:notes.delete,rpc:notes.list,rpc:visits.bump"
SC_ACTUAL_IDS="$(sc_all_rpc "$SC_ZSHIP")"
if [ "$SC_ACTUAL_IDS" = "$SC_EXPECT_IDS" ]; then
  pass "the template still publishes the six procedures this step drives"
else
  fail "the template's procedure set changed and the probes below no longer cover it.
      expected: $SC_EXPECT_IDS
      got     : $SC_ACTUAL_IDS
    Add a probe for each new id rather than comparing a subset."
fi

# --- 10c. Deploy it, and give it a schema the way a real deploy does --------
SC_OUT=$("$BIN/dev-provision" --db "$DB_URL" --blob-store /tmp/gp-bundles --name "$SC_APP" --zship "$SC_ZSHIP" 2>&1)
SC_APP_ID=$(echo "$SC_OUT" | awk -F= '$1 == "app_id" { print $2 }')
SC_API_KEY=$(echo "$SC_OUT" | awk -F= '$1 == "api_key" { print $2 }')
SC_READY=0
if [ -z "$SC_APP_ID" ] || [ -z "$SC_API_KEY" ]; then
  fail "could not provision the scaffold app: ${SC_OUT:0:200}"
else
  # OFFLINE-mint a platform-admin PAT signed with the key control+migrated
  # share. The .zship carries the DESCRIPTOR only; migrations travel through
  # zeroship-migrated, which is the real deployed path -- a hand-rolled CREATE
  # TABLE here would test nothing.
  # `apps:delete` is here for step 12 (teardown) and for nothing else. It was
  # ABSENT until 2026-08-11 and its absence read as a product defect: step 12's
  # DELETE came back `403 {"error":"forbidden"}` and the harness reported "a
  # creator cannot delete their own app", which would have been a serious and
  # entirely false finding. The token simply did not carry the action. The four
  # assertions after the DELETE all cascaded off that 403 and measured nothing.
  #
  # `deployments:read` is here for the same reason, added BEFORE the step that
  # needs it rather than after a run misread its absence. GET /api/apps/{id}/logs
  # -- the only surface a creator has for reading a deployed app's output --
  # requires Action::DeploymentsRead (crates/control/src/api.rs:2053), whose
  # string is "deployments:read" (crates/authz/src/action.rs:44). NONE of this
  # harness's PAT policies granted it, so the logs endpoint would have answered
  # 403 for every token here and the obvious reading of that 403 is "creators
  # cannot read their own logs". They can; the token could not ask. See #332.
  SC_POLICY='{"name":"golden-scaffold","statements":[{"effect":"allow","actions":["apps:read","apps:write","apps:deploy","apps:delete","deployments:read","billing:read","billing:write"],"resources":[{"type":"any"}],"conditions":[]}]}'
  SC_PHASH="$(node -e 'const{createHash}=require("crypto");function c(v){if(v===null||typeof v==="number"||typeof v==="boolean")return JSON.stringify(v);if(typeof v==="string")return JSON.stringify(v);if(Array.isArray(v))return "["+v.map(c).join(",")+"]";return "{"+Object.keys(v).sort().map(k=>JSON.stringify(k)+":"+c(v[k])).join(",")+"}";}process.stdout.write(createHash("sha256").update(c(JSON.parse(process.argv[1]))).digest("hex"));' "$SC_POLICY")"
  SC_CREATOR="$(node -e 'console.log(require("crypto").randomUUID())')"
  SC_TOKID="$(node -e 'console.log(require("crypto").randomUUID())')"
  SC_EXP=$(( $(date +%s) + 86400 ))
  docker exec -i "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.users (id,email,name,email_verified_at) VALUES ('$SC_CREATOR','golden-scaffold-$SC_CREATOR@zeroship.test'::citext,'Golden Scaffold',NOW());
INSERT INTO zeroship.platform_admin_roles (user_id,role,granted_by) VALUES ('$SC_CREATOR','admin','$SC_CREATOR');
INSERT INTO zeroship.permission_tokens (id,owner_id,kind,name,policies,policy_hash,expires_at) VALUES ('$SC_TOKID','$SC_CREATOR','pat','golden scaffold','$SC_POLICY'::jsonb,'$SC_PHASH',to_timestamp($SC_EXP));
INSERT INTO zeroship.app_members (app_id,user_id,role) VALUES ('$SC_APP_ID','$SC_CREATOR','owner') ON CONFLICT (app_id,user_id) DO UPDATE SET role='owner';
SQL
  GP_JOSE="$ROOT/node_modules/.pnpm/jose@6.2.3/node_modules/jose/dist/webapi/index.js"
  SC_PAT="$(node --input-type=module -e 'import{readFileSync}from "node:fs";import{createHash,randomBytes}from "node:crypto";import{importPKCS8,exportJWK,SignJWT}from "file://'"$GP_JOSE"'";const[pem,owner,tid,phash,exp]=process.argv.slice(1);const key=await importPKCS8(readFileSync(pem,"utf8"),"EdDSA",{extractable:true});const x=(await exportJWK(key)).x;const kid=createHash("sha256").update(`{"crv":"Ed25519","kty":"OKP","x":"${x}"}`).digest("base64url");const jwt=await new SignJWT({sub:owner,owner,tid,jti:tid,scope:"pat",policy_hash:phash,nonce:randomBytes(32).toString("base64url")}).setProtectedHeader({alg:"EdDSA",typ:"pat+jwt",kid}).setIssuer("https://api.zeroship.ai").setAudience("control.zeroship.ai").setIssuedAt(Math.floor(Date.now()/1000)).setExpirationTime(Number(exp)).sign(key);process.stdout.write(jwt);' "$GP_SIGNING_KEY" "$SC_CREATOR" "$SC_TOKID" "$SC_PHASH" "$SC_EXP" 2>/tmp/gp-scaffold-pat.log)"
  node --input-type=module - "$ROOT/sdks/vite-plugin/dist/gen-types/recorder.js" "$SCAFFOLD/migrations" >/tmp/gp-scaffold-ir.json 2>/tmp/gp-scaffold-ir.log <<'NODE'
import { pathToFileURL } from "node:url";
const [recorderPath, dir] = process.argv.slice(2);
const { discoverMigrations, recordMigration } = await import(pathToFileURL(recorderPath).href);
const migrations = await discoverMigrations(dir);
const documents = [];
for (const m of migrations) documents.push({ filename: m.stem + ".ir.json", body: await recordMigration(m.path) });
console.log(JSON.stringify({ kind: "ir", documents }));
NODE
  SC_APPLY_CODE="$(curl -s -o /tmp/gp-scaffold-apply.json -w '%{http_code}' -X POST \
    "http://localhost:$MIGRATED_PORT/v1/apps/$SC_APP_ID/migrations/apply" \
    -H 'Content-Type: application/json' -H "Authorization: Bearer $SC_PAT" \
    --data-binary @/tmp/gp-scaffold-ir.json)"
  SC_APPLIED="$(node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const o=JSON.parse(s);process.stdout.write(String((o.applied||[]).length))}catch{process.stdout.write("0")}})' </tmp/gp-scaffold-apply.json)"
  if [ "$SC_APPLY_CODE" = "200" ] && [ "${SC_APPLIED:-0}" -ge 1 ] 2>/dev/null; then
    pass "the scaffold's own migrations applied through zeroship-migrated (applied=$SC_APPLIED ops)"
    SC_READY=1
  else
    fail "zeroship-migrated could not apply the scaffold's migrations (http=$SC_APPLY_CODE): $(head -c 200 /tmp/gp-scaffold-apply.json)"
  fi
  sleep 5   # gateway route-sync poll
fi

# --- 10d/e. Drive the SAME six calls on both tiers and diff the RESULTS -----
#
# The generated @zeroship/rpc client mints a UUIDv7 Idempotency-Key for every
# `idempotent: true` write and sends it on BOTH tiers
# (sdks/rpc/src/transport.ts:131). A probe that omits it is not reproducing the
# creator's call: measured that way first, the gateway answered 400
# missing_idempotency_key while dev answered 200, and the "divergence" was the
# probe's own. It is supplied here for exactly the two ids the manifest marks
# idempotent, so the comparison is between two tiers, not between two clients.
sc_call() { # base auth-header method wireid body idem-key -> "<status> <body>"
  local base="$1" hdr="$2" method="$3" id="$4" body="${5:-}" idem="${6:-}" out st bd
  if [ "$method" = GET ]; then
    out=$(curl -s -m 25 -w '\n%{http_code}' ${hdr:+-H "$hdr"} "$base/__zeroship/v1/$id" 2>/dev/null)
  else
    out=$(curl -s -m 25 -w '\n%{http_code}' ${hdr:+-H "$hdr"} -X POST -H 'content-type: application/json' \
      ${idem:+-H "Idempotency-Key: $idem"} "$base/__zeroship/v1/$id" -d "$body" 2>/dev/null)
  fi
  st="${out##*$'\n'}"; bd="${out%$'\n'*}"
  printf '%s %s' "${st:-000}" "$(printf '%s' "$bd" | tr -d '\n' | head -c 300)"
}
sc_uuid() { node -e 'console.log(require("crypto").randomUUID())'; }
sc_tier() { # base auth-header outfile
  local base="$1" hdr="$2" out="$3"
  : > "$out"
  printf 'notes.list\t%s\n'   "$(sc_call "$base" "$hdr" GET  notes.list)" >> "$out"
  printf 'notes.add\t%s\n'    "$(sc_call "$base" "$hdr" POST notes.add '{"json":{"title":"golden path probe","body":"hello"}}' "$(sc_uuid)")" >> "$out"
  printf 'notes.delete\t%s\n' "$(sc_call "$base" "$hdr" POST notes.delete '{"json":{"id":"note_definitely_absent"}}')" >> "$out"
  printf 'files.upload\t%s\n' "$(sc_call "$base" "$hdr" POST files.upload '{"json":{"name":"probe.txt","dataBase64":"aGVsbG8="}}' "$(sc_uuid)")" >> "$out"
  printf 'files.list\t%s\n'   "$(sc_call "$base" "$hdr" GET  files.list)" >> "$out"
  printf 'visits.bump\t%s\n'  "$(sc_call "$base" "$hdr" POST visits.bump '{"json":{}}')" >> "$out"
}

SC_DEP_BASE="http://localhost:$GATE_PORT/apps/$SC_APP"
SC_DEV_BASE="http://localhost:$SC_V"
if [ "$SC_READY" -ne 1 ]; then
  fail "the deployed scaffold never became drivable; the dev-vs-deployed comparison did not run"
else
  # dev tier. `pnpm dev` no longer migrates (ee2c352aa), so the dev database is
  # created by the same separate step a creator now runs.
  rm -rf "$SCAFFOLD/.zeroship"
  free_ports "$SC_V" "$SC_RT"
  ( cd "$SCAFFOLD" && node "$ROOT/sdks/vite-plugin/dist/cli/migrate-dev.js" ) >/tmp/gp-scaffold-devmigrate.log 2>&1
  ( cd "$SCAFFOLD" && ./node_modules/.bin/vite --port "$SC_V" --strictPort ) >/tmp/gp-scaffold-dev.log 2>&1 &
  PIDS+=($!)
  SC_DEV_UP=0
  for _ in $(seq 1 90); do
    if [ "$(http_status "$SC_DEV_BASE/__zeroship/v1/notes.list")" != "000" ]; then SC_DEV_UP=1; break; fi
    sleep 1
  done
  if [ "$SC_DEV_UP" -ne 1 ]; then
    fail "the scaffold's dev server never answered on :$SC_V (dev half could not run): $(tail -3 /tmp/gp-scaffold-dev.log | tr '\n' ' ')"
  else
    pass "the scaffold runs under \`pnpm dev\` and answers its own RPCs"
    sc_tier "$SC_DEV_BASE" ""                        /tmp/gp-scaffold-dev.tsv
    sc_tier "$SC_DEP_BASE" "X-Api-Key: $SC_API_KEY"  /tmp/gp-scaffold-dep.tsv

    # THE COMPARISON. Statuses are compared exactly; bodies are not, because the
    # two tiers run different databases (SQLite vs Postgres) and every id and
    # timestamp in an answer is volatile BY CONSTRUCTION -- comparing them raw
    # would report a divergence on a platform behaving perfectly. What IS
    # compared beyond the status is the ENVELOPE KIND: a result (`{"json":`) or
    # a refusal (a bare `{"code":`/`{"message":`), plus the error code when
    # there is one. So "dev returned data, deployed returned an error" cannot
    # hide behind a matching status, and a 200 that carries an error envelope
    # cannot pass as agreement.
    sc_kind() { # body -> "ok" | "err:<CODE>" | "other"
      case "$1" in
        *'"json":'*) printf 'ok' ;;
        *'"code":'*) printf 'err:%s' "$(printf '%s' "$1" | sed -nE 's/.*"code":"([^"]+)".*/\1/p')" ;;
        *'"message":'*) printf 'err:<uncoded>' ;;
        *) printf 'other' ;;
      esac
    }
    while IFS=$'\t' read -r sc_id sc_dev; do
      sc_dep="$(grep -m1 "^$sc_id	" /tmp/gp-scaffold-dep.tsv | cut -f2)"
      sc_dev_st="${sc_dev%% *}"; sc_dev_bd="${sc_dev#* }"
      sc_dep_st="${sc_dep%% *}"; sc_dep_bd="${sc_dep#* }"
      sc_dev_v="$sc_dev_st $(sc_kind "$sc_dev_bd")"
      sc_dep_v="$sc_dep_st $(sc_kind "$sc_dep_bd")"
      if [ "$sc_dev_v" = "$sc_dep_v" ]; then
        pass "scaffold $sc_id: dev and deployed agree ($sc_dev_v)"
      else
        fail "scaffold $sc_id: dev and deployed DIVERGE -- dev=[$sc_dev_v] deployed=[$sc_dep_v]
      dev      : ${sc_dev_bd:0:150}
      deployed : ${sc_dep_bd:0:150}"
      fi
    done < /tmp/gp-scaffold-dev.tsv

    # --- 10f. THE CONTROL, and it differs in ONE variable ------------------
    #
    # Six identical verdicts are exactly what a harness that broke the deployed
    # tier for some unrelated reason would also produce -- a stale route, a
    # missing api key, a gateway that refuses this app whatever its manifest
    # says. So the same source is rebuilt with the policy posture FLIPPED and
    # redeployed to the SAME app id, over the SAME schema, through the SAME
    # gateway. Only the manifest's `auth` differs. If the deployed answers do
    # not move, the divergence above was never about policy and the six
    # failures should not be believed.
    #
    # The flip is written from the manifest, not hard-coded, so this control
    # keeps working after the operator decides: policy absent -> add anon;
    # policy present -> take it away.
    SC_CTL_DIR="$SCAFFOLD/src/server"
    if [ -n "$SC_UNPOLICED" ]; then
      mkdir -p "$SC_CTL_DIR"
      {
        echo 'import { defineApp } from "@zeroship/server";'
        echo 'export default defineApp({ resources: {'
        printf '%s\n' "$SC_ACTUAL_IDS" | tr ',' '\n' | while read -r rid; do
          [ -n "$rid" ] && echo "  \"$rid\": { auth: \"anon\", publiclyAccessible: true },"
        done
        echo '} });'
      } > "$SC_CTL_DIR/config.ts"
      SC_CTL_DESC="policy ADDED (anon)"
    else
      rm -rf "$SC_CTL_DIR"
      SC_CTL_DESC="policy REMOVED"
    fi
    if ( cd "$SCAFFOLD" && pnpm build ) >/tmp/gp-scaffold-ctlbuild.log 2>&1 \
       && "$BIN/zeroship" deploy "$SC_ZSHIP" --app="$SC_APP_ID" \
            --control="http://localhost:$CONTROL_PORT" --token="$SC_PAT" >/tmp/gp-scaffold-ctldeploy.log 2>&1; then
      sleep 6
      sc_tier "$SC_DEP_BASE" "X-Api-Key: $SC_API_KEY" /tmp/gp-scaffold-ctl.tsv
      SC_BEFORE="$(cut -f2 /tmp/gp-scaffold-dep.tsv | cut -d' ' -f1 | paste -sd, -)"
      SC_AFTER="$(cut -f2 /tmp/gp-scaffold-ctl.tsv | cut -d' ' -f1 | paste -sd, -)"
      # "The answers moved" is NOT enough, and asserting only that is how this
      # control passed on its first run over a stack with a DEAD WORKER: the
      # flip turned 401,401,401,401,401,401 into 502,502,502,502,502,502 and the
      # check reported success. A 5xx is the stack failing, and a control that
      # cannot tell a policy change from an outage discriminates nothing. So the
      # control run must contain no 5xx at all, AND the set of 401s must move.
      SC_CTL_5XX="$(cut -f2 /tmp/gp-scaffold-ctl.tsv | cut -d' ' -f1 | grep -c '^5' || true)"
      SC_401_BEFORE="$(awk -F'\t' '{split($2,a," "); if (a[1]=="401") print $1}' /tmp/gp-scaffold-dep.tsv | paste -sd, -)"
      SC_401_AFTER="$(awk -F'\t' '{split($2,a," "); if (a[1]=="401") print $1}' /tmp/gp-scaffold-ctl.tsv | paste -sd, -)"
      if [ "$SC_CTL_5XX" -gt 0 ]; then
        fail "CONTROL is UNINFORMATIVE: $SC_CTL_5XX of 6 control answers are 5xx ($SC_AFTER).
      That is the stack failing, not the policy taking effect, so it cannot tell a
      gateway honouring the manifest from one that is simply broken. The six
      verdicts above are UNPROVEN until this control runs on a healthy stack."
      elif [ "$SC_401_BEFORE" != "$SC_401_AFTER" ]; then
        pass "CONTROL: flipping ONLY the policy ($SC_CTL_DESC) moved the 401 set (${SC_401_BEFORE:-<none>} -> ${SC_401_AFTER:-<none>}), no 5xx"
      else
        fail "CONTROL: $SC_CTL_DESC left the 401 set unchanged (${SC_401_AFTER:-<none>}); statuses $SC_BEFORE -> $SC_AFTER.
      The gateway is not discriminating on the manifest policy in this harness, so
      the six verdicts above measure something else and must not be read as the
      fail-closed default firing."
      fi
    else
      fail "CONTROL could not be built or deployed, so the verdicts above are unproven:
      build: $(tail -2 /tmp/gp-scaffold-ctlbuild.log | tr '\n' ' ')
      deploy: $(tail -2 /tmp/gp-scaffold-ctldeploy.log | tr '\n' ' ')"
    fi
    rm -rf "$SC_CTL_DIR"
  fi
fi

# --- 11. `sort({ id })` must be creation order on BOTH tiers (#236 / #255) ---
#
# THE CLAIM UNDER TEST, and it is ABSOLUTE, not a tier disagreement. A typed id
# is `<prefix>_<base62(uuidv7)>`, so its BYTE order IS its creation order --
# that is the entire reason `sort({ id: -1 })` is spelled "newest first" in
# every example and doc we ship. Whether a backend honours that depends on the
# collation the `id` column sorts under. SQLite sorts `BINARY`. Postgres sorts
# under the DATABASE collation, `en_US.utf8`, which orders base62's uppercase
# and lowercase runs differently from bytes. So dev is right and deployed is
# wrong, and each tier is judged against the byte order of its OWN ids rather
# than against the other tier -- a relative check alone would call two
# identically-broken tiers "agreed".
#
# WHY THIS IS CONSTRUCTED AND NOT SAMPLED, which is the whole difficulty. The
# base62 digit that discriminates two ids encodes the clock, so it sweeps the
# alphabet as real time passes: for whole stretches of wall-clock the
# discriminating characters are one case (order agrees, any assertion passes)
# and for whole stretches they are mixed case (order disagrees, the same
# assertion fails). Measured: ids 100ms apart diverged in 42 of 50 batches with
# adjacent batches agreeing 45/49 against 35.8 expected under independence
# (docs/pilot/e2e-scenarios.md, "#236 collation divergence"). A test that mints
# six rows and checks their order is therefore GREEN MOST OF THE TIME AT HEAD,
# and re-running a red is NOT evidence it was spurious. #239 is exactly that
# trap.
#
# So this step does not assert on a hoped-for property. It mints a population,
# then PROVES the population is discriminating before asserting on it: at least
# one pair of ids must sort differently under bytes than under `en_US`. If no
# such pair exists the step FAILS -- loudly, as "could not construct the case"
# -- rather than reporting a green that means nothing.
#
# THE `en_US` MODEL IS NOT MINE, IT IS MEASURED. `Intl.Collator("en-US")` is
# used as the stand-in because it needs no host locale. It was checked against
# both of the things it stands for, on this exact id shape (2026-08-10):
#
#   ids           todo_...K19  todo_...K1Z  todo_...K1a  todo_...K1z
#   byte / C      19 1Z 1a 1z
#   PG en_US.utf8 19 1a 1z 1Z   (docker exec psql, ORDER BY id)
#   glibc sort    19 1a 1z 1Z   (LC_ALL=en_US.UTF-8 sort)
#   Intl.Collator 19 1a 1z 1Z
#
# All three non-byte orderings agree, so the guard below models Postgres rather
# than merely modelling itself.
#
# The pairwise sector must stay DERIVED FROM THE APP, not constant.
#
# Two apps must not be able to correlate the same human. That rests on
# `derive_pairwise(salt, user, sector)` being fed a per-app `sector`
# (crates/core/src/auth/mod.rs:304). The derivation itself is unit-tested for
# distinctness (`assert_ne!` on two sectors, same file), and the sector is
# written once at registration as the app's apex origin
# (crates/control/src/app_oauth_client.rs:150, inserted at :592, immutable
# afterwards by trigger).
#
# THE UNCOVERED LINK WAS REGISTRATION, and its failure is silent: make
# `sector_identifier` return a constant and NOTHING goes red today. The `pws_`
# format check still passes, and every harness runs one app, so no comparison
# exists to break. This asserts the property that survives with one app - the
# sector must CONTAIN the app's own name, which a constant cannot.
#
# WHAT THIS DOES NOT CATCH, stated because it is weaker than the real property:
# it does not compare two apps. It infers distinctness from app-derivedness,
# which holds only because app names are unique. A two-app comparison needs a
# real OP and belongs in tests/e2e_dev_vs_deployed_auth.sh (#328); this gateway
# has no database at all and cannot do OIDC.
# A SECOND app, registered through the DEPLOY API, so there are two rows to
# compare rather than one to characterise.
#
# Only the deploy API calls `ensure_app_client`; `dev-provision` never does
# (crates/control/src/bin/dev_provision.rs:106, zero references to it), which is
# why three of this harness's four apps have no client row at all and the
# check below would otherwise have exactly one to look at. The PAT minted for
# step 10's control already carries apps:write + apps:deploy on {"type":"any"},
# so no new credential is needed.
#
# A THROWAWAY APP ON PURPOSE: do NOT reuse db-todos for the second row. Step 11
# asserts against its deployed state and re-deploying it here would change the
# thing that step measures.
if [ -z "${SC_PAT:-}" ]; then
  fail "SC_PAT is unset, so the second app cannot be registered - the distinctness check below would compare one row against itself"
else
  PW_APP="gppairwise"
  PW_JSON=$(curl -sf -X POST "http://localhost:$CONTROL_PORT/api/apps" \
    -H 'Content-Type: application/json' -H "Authorization: Bearer $SC_PAT" \
    -d "{\"name\":\"$PW_APP\"}" 2>/tmp/gp-pairwise-create.log)
  # NO jq HERE, deliberately. The CI job for this harness installs lsof, zstd
  # and postgresql-client and nothing else, and this file has no prerequisite
  # check at all - so a missing tool fails somewhere in the middle with no
  # diagnosis. The one other jq use (step 3) sits inside the `if [ -n "$TOKEN" ]`
  # branch, which CI never takes, so this block would have been the first
  # unconditional dependency on it. Whether the runner image happens to ship jq
  # is not something worth resting on.
  #
  # Anchored on the QUOTED key rather than a greedy `.*"id"`, and tested against
  # a payload where `owner_id` follows `id`, the spaced `{ "id" : ... }` variant,
  # and an error body with no id at all - which must yield empty so the fail arm
  # below fires rather than deploying to nothing.
  PW_ID=$(printf '%s' "$PW_JSON" | grep -oE '"id"[[:space:]]*:[[:space:]]*"[^"]+"' | head -1 | sed 's/.*"\([^"]*\)"$/\1/')
  if [ -z "$PW_ID" ]; then
    fail "could not create the second app for the pairwise check: $(head -c 200 <<<"$PW_JSON")"
  else
    "$BIN/zeroship" deploy "$SC_ZSHIP" --app="$PW_ID" \
      --control="http://localhost:$CONTROL_PORT" --token="$SC_PAT" \
      >/tmp/gp-pairwise-deploy.log 2>&1 \
      && pass "second app registered through the deploy API ($PW_APP)" \
      || fail "second app deploy failed: $(tail -2 /tmp/gp-pairwise-deploy.log | tr '\n' ' ')"
  fi
fi

sector_rows=$(docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -tAc \
  "select count(*) from zeroship.app_oauth_clients" 2>/dev/null | tr -d '[:space:]')
# THE PROPERTY ITSELF, now that two apps exist: distinct apps must carry
# DISTINCT sectors. The app-derivedness check below is weaker - it implies
# distinctness only because names happen to be unique - so this measures what
# that one infers. Requires >= 2 rows or it is vacuous, which the count above
# and the guard here together enforce.
sector_distinct=$(docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -tAc \
  "select count(distinct sector_identifier) from zeroship.app_oauth_clients" 2>/dev/null | tr -d '[:space:]')
[ "${sector_rows:-0}" -ge 2 ] && [ "$sector_rows" = "$sector_distinct" ] \
  && pass "each app carries a DISTINCT pairwise sector ($sector_distinct distinct across $sector_rows apps)" \
  || fail "sectors are not distinct per app ($sector_distinct distinct across $sector_rows apps; needs >= 2 rows and all distinct): two apps would derive the SAME pairwise subject for one human (#328)"
sector_ok=$(docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -tAc \
  "select count(*) from zeroship.app_oauth_clients c join zeroship.apps a on a.id = c.app_id where position(a.name in c.sector_identifier) > 0" 2>/dev/null | tr -d '[:space:]')
# The row count is asserted separately: with zero clients the equality below is
# 0 = 0 and would read as a pass over nothing.
[ "${sector_rows:-0}" -ge 1 ] \
  && pass "there is at least one registered app OAuth client to check ($sector_rows)" \
  || fail "no rows in zeroship.app_oauth_clients - the sector assertion below would pass vacuously"
[ "$sector_rows" = "$sector_ok" ] \
  && pass "every app's pairwise sector is derived from its own name ($sector_ok of $sector_rows)" \
  || fail "$((sector_rows - sector_ok)) of $sector_rows app OAuth clients carry a sector that does not contain the app name: two apps could derive the SAME pairwise subject and correlate a user (#328)"

# WHY db-todos AND NOT THE SCAFFOLD. Step 10's app answers 401 deployed (it
# ships no policy), so no env.db operation of it is observable on the deployed
# tier. db-todos is anon by policy, has migrations, and its `todos.list` already
# sorts `{ id: -1 }` -- the surface a creator would actually use.
step 11 "env.db ordering: sort({id}) is creation order on both tiers"
DB_APP="dbtodos"
DB_ZSHIP="$TODOS/dist/app.zship"
DB_DEP_READY=0

if [ -z "${SC_PAT:-}" ] || [ -z "${SC_CREATOR:-}" ]; then
  fail "step 10 minted no PAT, so the deployed half of the ordering check cannot run"
elif ! ( cd "$TODOS" && pnpm build ) >/tmp/gp-dbtodos-build.log 2>&1 || [ ! -f "$DB_ZSHIP" ]; then
  fail "examples/db-todos does not build: $(tail -5 /tmp/gp-dbtodos-build.log | tr '\n' ' ')"
else
  pass "db-todos builds through the real vite-plugin ($(du -k "$DB_ZSHIP" | cut -f1)KB)"
  DB_OUT=$("$BIN/dev-provision" --db "$DB_URL" --blob-store /tmp/gp-bundles --name "$DB_APP" --zship "$DB_ZSHIP" 2>&1)
  DB_APP_ID=$(echo "$DB_OUT" | awk -F= '$1 == "app_id" { print $2 }')
  DB_API_KEY=$(echo "$DB_OUT" | awk -F= '$1 == "api_key" { print $2 }')
  if [ -z "$DB_APP_ID" ] || [ -z "$DB_API_KEY" ]; then
    fail "could not provision db-todos: ${DB_OUT:0:200}"
  else
    # Same creator as step 10, so the PAT already minted is accepted; only the
    # membership row is per-app. Migrations travel through zeroship-migrated --
    # a hand-rolled CREATE TABLE here would create the table with whatever
    # collation THIS script chose, which is precisely the thing under test.
    docker exec -i "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.app_members (app_id,user_id,role) VALUES ('$DB_APP_ID','$SC_CREATOR','owner') ON CONFLICT (app_id,user_id) DO UPDATE SET role='owner';
SQL
    node --input-type=module - "$ROOT/sdks/vite-plugin/dist/gen-types/recorder.js" "$TODOS/migrations" >/tmp/gp-dbtodos-ir.json 2>/tmp/gp-dbtodos-ir.log <<'NODE'
import { pathToFileURL } from "node:url";
const [recorderPath, dir] = process.argv.slice(2);
const { discoverMigrations, recordMigration } = await import(pathToFileURL(recorderPath).href);
const migrations = await discoverMigrations(dir);
const documents = [];
for (const m of migrations) documents.push({ filename: m.stem + ".ir.json", body: await recordMigration(m.path) });
console.log(JSON.stringify({ kind: "ir", documents }));
NODE
    DB_APPLY_CODE="$(curl -s -o /tmp/gp-dbtodos-apply.json -w '%{http_code}' -X POST \
      "http://localhost:$MIGRATED_PORT/v1/apps/$DB_APP_ID/migrations/apply" \
      -H 'Content-Type: application/json' -H "Authorization: Bearer $SC_PAT" \
      --data-binary @/tmp/gp-dbtodos-ir.json)"
    DB_APPLIED="$(node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const o=JSON.parse(s);process.stdout.write(String((o.applied||[]).length))}catch{process.stdout.write("0")}})' </tmp/gp-dbtodos-apply.json)"
    if [ "$DB_APPLY_CODE" = "200" ] && [ "${DB_APPLIED:-0}" -ge 1 ] 2>/dev/null; then
      pass "db-todos migrations applied through zeroship-migrated (applied=$DB_APPLIED ops)"
      DB_DEP_READY=1
      sleep 6   # gateway route-sync poll
    else
      fail "zeroship-migrated could not apply db-todos' migrations (http=$DB_APPLY_CODE): $(head -c 200 /tmp/gp-dbtodos-apply.json)"
    fi
  fi
fi

# One RPC helper for both tiers: same method, same body, only the base URL and
# the api-key header differ.
ord_call() { # base auth-header wireid json -> body
  local base="$1" hdr="$2" id="$3" body="$4"
  curl -s -m 25 ${hdr:+-H "$hdr"} -X POST -H 'content-type: application/json' \
    "$base/__zeroship/v1/$id" -d "{\"json\":$body}" 2>/dev/null
}
ord_id() { printf '%s' "$1" | sed -nE 's/.*"id":"([^"]+)".*/\1/p' | head -1; }

# Mint ORD_N rows through `todos.create` (the platform mints every id -- nothing
# here supplies one) and print the ids one per line, in creation order.
#
# ORD_N IS A MEASUREMENT, and ORD_ROUNDS is why it is not the whole story. A
# population can only expose a collation defect if two of its ids sort
# differently under bytes than under `en_US`, and how often that happens depends
# entirely on how many you mint (5000 simulated populations at the observed mint
# cadence):
#
#     ids     2      6     12     24     48
#     disc  14.7%  58.8%  88.5%  99.1%  100.0%
#
# #239 minted SIX. Four runs in ten of that fixture could not have failed however
# broken the backend was, which is why its red looked spurious on re-run.
#
# 24 IS NOT ENOUGH EITHER, and this is measured on THIS gate rather than
# simulated: in 2 of 5 four-service runs the DEPLOYED population of 24 was NOT
# discriminating. In the run before the top-up existed, that printed
# `deployed: ordered` and `dev and deployed AGREE` over a platform two other
# runs had just proved wrong -- the false green this whole step exists to
# prevent, reproduced live. So the population TOPS UP until it can discriminate,
# and only then is anything asserted about ordering. The run after the top-up
# landed hit the same non-discriminating draw, minted 24 more, and reported the
# real red; the note is left in the log (`... cannot discriminate ...; minting 24
# more`) so a reader can see when it fired.
ORD_N=24
ORD_ROUNDS=4
ord_mint() { # base auth-header userid -> ids, one per line
  local base="$1" hdr="$2" uid="$3" i out
  for i in $(seq 1 "$ORD_N"); do
    out="$(ord_call "$base" "$hdr" todos.create "{\"userId\":\"$uid\",\"title\":\"ord-$i\"}")"
    # `printf '%s\n'`, not a bare `ord_id`: the sed inside it inherits its
    # input's missing trailing newline, so 24 ids would arrive as one 648-byte
    # line and the verdict would read a population of 1. Measured, not guessed.
    printf '%s\n' "$(ord_id "$out")"
  done
}

# The whole verdict for one tier, computed in node from two inputs: the ids in
# CREATION order and the ids in the order `todos.list` returned them.
#
#   discriminating  does this population contain a pair that bytes and en_US
#                   order differently? If not, the assertion below cannot fail
#                   however broken the backend is, and the step says so.
#   ordered         did `sort({id:-1})` return byte-DESCENDING order?
#
# Creation order is printed too, but is NOT what `ordered` is judged against:
# `uuid::Uuid::now_v7` randomises the low 74 bits, so two ids minted inside one
# millisecond may legitimately come back in either order. Byte order is the
# claim -- it is what makes an id time-ordered in the first place.
ord_verdict() { # creation-order-file returned-order-file -> "<ordered> <discriminating> <detail>"
  node -e '
const fs = require("fs");
const [mintedF, gotF] = process.argv.slice(1);
const rd = (f) => fs.readFileSync(f, "utf8").split("\n").map((s) => s.trim()).filter(Boolean);
const minted = rd(mintedF), got = rd(gotF);
if (minted.length === 0) { console.log("no-ids no-ids nothing was minted"); process.exit(0); }
if (got.length !== minted.length) {
  console.log(`readback-mismatch unknown minted ${minted.length}, list returned ${got.length}`);
  process.exit(0);
}
const byteDesc = [...minted].sort().reverse();
const coll = new Intl.Collator("en-US");
const localeDesc = [...minted].sort(coll.compare).reverse();
const discriminating = JSON.stringify(byteDesc) !== JSON.stringify(localeDesc);
const ordered = JSON.stringify(got) === JSON.stringify(byteDesc);
// Name the first place the returned order departs from byte order, with the
// characters that decide it -- that is what turns a red into a diagnosis.
let detail = "byte order";
if (!ordered) {
  const i = got.findIndex((v, k) => v !== byteDesc[k]);
  const a = got[i] ?? "", b = byteDesc[i] ?? "";
  let p = 0; while (p < a.length && a[p] === b[p]) p++;
  detail = `position ${i}: got ${a} expected ${b} (differ at char ${p}: ` +
           `${JSON.stringify(a[p] ?? "")} vs ${JSON.stringify(b[p] ?? "")})`;
}
console.log(`${ordered ? "ordered" : "NOT-ordered"} ${discriminating ? "discriminating" : "NOT-discriminating"} ${detail}`);
' "$1" "$2"
}

# Drive one tier end to end. Kept as a function so the two tiers cannot drift
# into running different sequences -- the single most common way a
# dev-vs-deployed comparison stops comparing.
ord_discriminating() { # ids-file -> exit 0 when bytes and en_US order it differently
  node -e '
const fs = require("fs");
const ids = fs.readFileSync(process.argv[1], "utf8").split("\n").map((s) => s.trim()).filter(Boolean);
const coll = new Intl.Collator("en-US");
const differs = JSON.stringify([...ids].sort()) !== JSON.stringify([...ids].sort(coll.compare));
process.exit(differs ? 0 : 1);
' "$1"
}

ord_tier() { # label base auth-header outfile -> writes "<ordered> <discriminating> <detail>"
  local label="$1" base="$2" hdr="$3" out="$4" tag seed uid round
  tag="ord-$$-${RANDOM}"
  seed="$(ord_call "$base" "$hdr" users.seed "{\"email\":\"$tag@example.com\",\"name\":\"Ord\",\"handle\":\"$tag\"}")"
  uid="$(ord_id "$seed")"
  if [ -z "$uid" ]; then
    printf 'seed-failed seed-failed %s\n' "${seed:0:150}" > "$out"
    return 1
  fi
  : > "/tmp/gp-ord-$label.minted"
  for round in $(seq 1 "$ORD_ROUNDS"); do
    ord_mint "$base" "$hdr" "$uid" >> "/tmp/gp-ord-$label.minted"
    ord_discriminating "/tmp/gp-ord-$label.minted" && break
    echo "  $label: $(( round * ORD_N )) ids cannot discriminate a collation; minting $ORD_N more"
  done
  ord_call "$base" "$hdr" todos.list "{\"userId\":\"$uid\"}" \
    | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const o=JSON.parse(s);const r=o.json??o;if(!Array.isArray(r))throw 0;process.stdout.write(r.map(x=>x.id).join("\n"))}catch{process.stdout.write("")}})' \
    > "/tmp/gp-ord-$label.got"
  ord_verdict "/tmp/gp-ord-$label.minted" "/tmp/gp-ord-$label.got" > "$out"
}

ORD_DEV_BASE="http://localhost:$DB_RT"
ORD_DEP_BASE="http://localhost:$GATE_PORT/apps/$DB_APP"
ORD_DEV_V=""; ORD_DEP_V=""

if [ "$(http_status "$ORD_DEV_BASE/__zeroship/v1/todos.list")" = "000" ]; then
  fail "the db-todos dev runtime from step 9 is not answering on :$DB_RT; the dev half of the ordering check did not run"
else
  ord_tier dev "$ORD_DEV_BASE" "" /tmp/gp-ord-dev.verdict
  ORD_DEV_V="$(cat /tmp/gp-ord-dev.verdict)"
  echo "  dev      : $ORD_DEV_V"
  case "$ORD_DEV_V" in
    *NOT-discriminating*)
      fail "dev: even $((ORD_ROUNDS * ORD_N)) minted ids contain NO pair that bytes and en_US order
      differently, so the ordering assertion could not have failed however broken the
      backend was. This is a FAILED SETUP, not a passing check. If it recurs, the id
      alphabet or the mint cadence changed and this probe needs rebuilding." ;;
    ordered*)
      pass "dev: sort({id:-1}) returned byte order over a population that discriminates collations" ;;
    *)
      fail "dev: sort({id:-1}) did NOT return byte order -- $ORD_DEV_V" ;;
  esac
fi

if [ "$DB_DEP_READY" -ne 1 ]; then
  fail "db-todos never became drivable through the gateway; the deployed half of the ordering check did not run"
else
  ord_tier dep "$ORD_DEP_BASE" "X-Api-Key: $DB_API_KEY" /tmp/gp-ord-dep.verdict
  ORD_DEP_V="$(cat /tmp/gp-ord-dep.verdict)"
  echo "  deployed : $ORD_DEP_V"
  case "$ORD_DEP_V" in
    *NOT-discriminating*)
      fail "deployed: even $((ORD_ROUNDS * ORD_N)) minted ids contain NO pair that bytes and en_US
      order differently, so the ordering assertion could not have failed. FAILED SETUP,
      not a pass." ;;
    ordered*)
      pass "deployed: sort({id:-1}) returned byte order over a population that discriminates collations" ;;
    *)
      fail "deployed: sort({id:-1}) is NOT creation order -- $ORD_DEP_V
      The deployed \`id\` column is created by zeroship-migrated and inherits the
      database collation (en_US.utf8); SQLite sorts BINARY. Fix is COLLATE \"C\"
      on typed-id text columns in the DDL. See #255 and
      docs/reference/sqlite-divergences.md." ;;
  esac
fi

# The RELATIVE half, which is the seam this file exists for. Reported separately
# from the two absolute verdicts above because it answers a different question,
# and because a defect BOTH tiers shared would leave this line green.
if [ -n "$ORD_DEV_V" ] && [ -n "$ORD_DEP_V" ]; then
  if [ "${ORD_DEV_V%% *}" = "${ORD_DEP_V%% *}" ]; then
    pass "dev and deployed AGREE on id ordering (${ORD_DEV_V%% *})"
  else
    fail "dev and deployed DIVERGE on id ordering -- dev=${ORD_DEV_V%% *} deployed=${ORD_DEP_V%% *}
      dev      : $ORD_DEV_V
      deployed : $ORD_DEP_V"
  fi
fi

gp_close_step

# --- 13. The creator reads their deployed app's logs ------------------------
#
# "My app misbehaved, show me why" is an ordinary creator step and had no row
# in the spine. GET /api/apps/{id}/logs is a real surface -- control fans out to
# every worker (api.rs:2061-2074) and each worker keeps a 1000-line in-memory
# ring per app (worker/src/logs.rs) fed from FetchOutcome.logs on seven dispatch
# paths. Its ONLY test served /logs/{app_id} from a MOCK worker defined in the
# test file, so the seam that matters -- real isolate output reaching a real
# ring and coming back out -- was untested by construction, and no e2e harness
# had ever called the endpoint. See #332.
#
# THE VEHICLE had to be built: NO app on the deploy path emitted anything.
# examples/starter now logs one line in `getMessages` (d7c3d4cce), chosen
# because it is `auth: "anon", publiclyAccessible` and so runs on BOTH tiers;
# addMessage is default-user and 401s deployed, which would have manufactured a
# divergence rather than found one.
#
# THE TWO ARMS ARE NOT SYMMETRIC, deliberately, and this is the honest shape
# rather than a shortcut. The dev server is killed at the end of step 6, so the
# dev arm cannot drive a fresh call; it reads what step 6 already produced,
# which is exactly the terminal output a creator sees. The deployed arm CAN
# drive a fresh call, so it uses the stronger before/after discriminator: steps
# 4-6 have already populated the ring, so "the marker is present" would pass
# over a stale buffer. Only an INCREASE proves this call reached it.
step 13 "Logs: the creator can read what their deployed app printed"
GP_LOG_MARK="[starter] getMessages"
if [ -z "${SC_PAT:-}" ] || [ -z "${APP_ID:-}" ]; then
  fail "no PAT or app id, so the logs surface could not be exercised at all --
      FAILED SETUP, not a passing check"
else
  # Harness setup, not a product claim: give the PAT's principal membership on
  # the starter app so the token is authorized here the same way step 10c does
  # for the scaffold. The POLICY already carries deployments:read (1974406b0).
  docker exec -i "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.app_members (app_id,user_id,role) VALUES ('$APP_ID','$SC_CREATOR','owner') ON CONFLICT (app_id,user_id) DO UPDATE SET role='owner';
SQL

  # --- DEV arm: what step 6 already printed to the creator's terminal --------
  LOG_DEV=$(grep -cF -- "$GP_LOG_MARK" /tmp/gp-dev.log 2>/dev/null || echo 0)
  echo "  dev (/tmp/gp-dev.log): $LOG_DEV line(s) carrying the marker"
  if [ "${LOG_DEV:-0}" -ge 1 ] 2>/dev/null; then
    pass "dev: the app's server-side log line reached the creator's terminal"
  else
    fail "dev: step 6 drove getMessages on the dev tier and PASSED, but the app's
      log line never appeared in /tmp/gp-dev.log.
      THE CAPTURE PATH IS NOT THE EXPLANATION, and that was checked rather than
      assumed: the same file carries the runtime's own startup lines forwarded
      under the \`[zeroship:api]\` prefix that sdks/vite-plugin/src/dev-server.ts
      :967 attaches to the child's stdout. So the terminal is receiving what the
      runtime prints; the app's per-request console output is simply not among
      it. The runtime collects per-request output into FetchOutcome.logs, which
      the deployed worker appends to its ring buffer -- in dev there may be no
      consumer for that vec at all. Confirm the vehicle first
      (examples/starter/src/server.ts still contains the marker) and only then
      read this as the dev tier discarding what a creator printed."
  fi

  # --- DEPLOYED arm: before, drive one call, after ---------------------------
  LOG_URL="http://localhost:$CONTROL_PORT/api/apps/$APP_ID/logs"
  LOG_CODE_A=$(curl -s -o /tmp/gp-logs-a.json -w '%{http_code}' --max-time 20 \
    "$LOG_URL" -H "Authorization: Bearer $SC_PAT" 2>/dev/null)
  LOG_BEFORE=$(grep -oF -- "$GP_LOG_MARK" /tmp/gp-logs-a.json 2>/dev/null | wc -l | tr -d ' ')

  if [ "$LOG_CODE_A" = "200" ]; then
    pass "the logs endpoint answers for the app's owner (HTTP 200)"
  else
    fail "GET /api/apps/<id>/logs returned $LOG_CODE_A for the app's OWNER: $(head -c 200 /tmp/gp-logs-a.json)
      A creator with no way to read their deployed app's output has no
      workaround, so this is a finding about the creator path."
  fi

  curl -s -o /dev/null --max-time 15 \
    "http://localhost:$GATE_PORT/apps/$APP_NAME/__zeroship/v1/getMessages" \
    -H "X-Api-Key: $API_KEY" 2>/dev/null || true
  command sleep 2

  LOG_CODE_B=$(curl -s -o /tmp/gp-logs-b.json -w '%{http_code}' --max-time 20 \
    "$LOG_URL" -H "Authorization: Bearer $SC_PAT" 2>/dev/null)
  LOG_AFTER=$(grep -oF -- "$GP_LOG_MARK" /tmp/gp-logs-b.json 2>/dev/null | wc -l | tr -d ' ')
  echo "  deployed (GET /api/apps/<id>/logs): http=$LOG_CODE_A/$LOG_CODE_B marker $LOG_BEFORE -> $LOG_AFTER"

  if [ "${LOG_AFTER:-0}" -gt "${LOG_BEFORE:-0}" ] 2>/dev/null; then
    pass "deployed: driving getMessages ADDED a log line the creator can read ($LOG_BEFORE -> $LOG_AFTER)"
  elif [ "${LOG_AFTER:-0}" -ge 1 ] 2>/dev/null; then
    fail "deployed: the marker is in the log buffer ($LOG_AFTER) but driving getMessages
      did not increase it ($LOG_BEFORE -> $LOG_AFTER). The buffer is stale: this
      run cannot tell a working capture path from one that stopped, which is
      why the count and not the presence is the assertion."
  else
    fail "deployed: the app printed a line on every getMessages and the creator's
      log surface shows NONE of it (marker $LOG_BEFORE -> $LOG_AFTER, http=$LOG_CODE_B).
      Body: $(head -c 200 /tmp/gp-logs-b.json)"
  fi

  # --- DOES AN ERROR REACH THE CREATOR TOO, or only what they printed? ------
  #
  # The half of observability that matters most is the half you did not choose
  # to emit. sdks/bootstrap/src/fetch-handler.ts:381 logs
  # `console.error("[zeroship:rpc] sanitized error", ...)` on the RPC error
  # path, and crates/runtime/src/core/init.rs:3214-3225 binds log/warn/error/
  # info/debug to the SAME console_log_callback, which pushes into
  # `per_request_logs` -- so there is no stdout/stderr split in the runtime and
  # an error line should travel exactly the route the success line just did.
  # That is READ, not run, which is why this arm exists.
  #
  # WHAT THIS DRIVES IS AN INPUT-REJECTION, NOT A HANDLER THROW. getMessages is
  # the only anon procedure and takes no input, so garbage in `?input=` is the
  # error class reachable without adding a procedure. A genuine uncaught throw
  # is still untested and needs its own vehicle (#333) -- so a red here is
  # informative and a green here does NOT license "errors reach the creator"
  # in general.
  # THE STATUS IS CAPTURED, and that is not decoration. The first version of this
  # arm sent the response to /dev/null and reported 0 error lines -- which cannot
  # distinguish "the error rail does not log" from "I never triggered an error".
  # getMessages declares no input schema, so a stray `?input=` may simply be
  # ignored and the request may SUCCEED. A red that proves nothing is worse than
  # no arm, so the status now gates the reading below.
  LOG_ERR_CODE=$(curl -s -o /tmp/gp-err-resp.json -w '%{http_code}' --max-time 15 \
    "http://localhost:$GATE_PORT/apps/$APP_NAME/__zeroship/v1/getMessages?input=%7Bnot-json" \
    -H "X-Api-Key: $API_KEY" 2>/dev/null)
  command sleep 2
  curl -s -o /tmp/gp-logs-e.json --max-time 20 "$LOG_URL" \
    -H "Authorization: Bearer $SC_PAT" 2>/dev/null || true
  LOG_ERR=$(grep -oF -- "[zeroship:rpc] sanitized error" /tmp/gp-logs-e.json 2>/dev/null | wc -l | tr -d ' ')
  echo "  deployed error probe: http=$LOG_ERR_CODE, rpc error marker x$LOG_ERR"
  if [ "${LOG_ERR_CODE:-200}" -lt 400 ] 2>/dev/null; then
    fail "the error probe did NOT produce an error: the request returned
      http=$LOG_ERR_CODE. getMessages declares no input schema, so the stray
      \`?input=\` was very likely ignored and this drove a SUCCESS. A zero
      error-line count here says nothing about whether the error rail reaches
      the creator -- it is a FAILED SETUP, not a finding. A genuine uncaught
      throw needs its own anon procedure (#333); note golden_path.sh:334 asserts
      the manifest ids are exactly rpc:addMessage,rpc:boom,rpc:getMessages, so that
      assertion moves with it."
  elif [ "${LOG_ERR:-0}" -ge 1 ] 2>/dev/null; then
    pass "deployed: an RPC error also reaches the creator's log surface (input-rejection class)"
  else
    fail "deployed: a request REJECTED with $LOG_ERR_CODE left the creator no log line.
      READ THIS AS WHAT IT IS, NOT AS A DELIVERY FINDING. This arm observes what
      a creator SEES (nothing) and does NOT establish that the log rail is
      broken, because it cannot show any JS ran. sdks/bootstrap/src/dispatcher.ts
      :55-58 records that an unparseable body is rejected by the RUST parser
      (crates/runtime/src/core/runtime.rs::parse_rpc_body) before the JS
      dispatcher, and a request that never reached JS printed nothing, so
      nothing arriving is expected rather than symptomatic. CAVEAT, unresolved:
      that comment is about a BODY and this probe uses the query string, so
      which layer rejected THIS request is unverified.
      THE DIAGNOSTIC ARM IS THE THROW BELOW, where the isolate demonstrably ran.
      Its MECHANISM IS UNIDENTIFIED (#334) and this line used to name
      handler.rs:459 as the root cause. That attribution was never supported and
      has now outlived three hypotheses killed by measurement: (1) the worker's
      Some(Err(e)) arm -- threading the logs through it left this red exactly as
      red, and SettledResult::Rpc's Fetch arm is unreachable!() for fetch/RPC
      anyway; (2) key-0 orphaning at init.rs:2523's unwrap_or(0) -- refuted, a
      throwing request's line is captured under a real req_id; (3) capture
      failure -- refuted by the same probe. What IS established: the loss is
      downstream of capture. Do not restore a named cause here without a run."
  fi

  # --- THE ERROR CLASS THAT IS THE CREATOR'S OWN BUG ------------------------
  #
  # The 400 arm above cannot separate "the error rail does not deliver" from
  # "no JS ever ran", because an input rejection is refused BEFORE the handler.
  # `boom` throws INSIDE the handler, so the isolate demonstrably executed.
  #
  # THE PREDICTION IS RESOLVED, and half of it was wrong. It read: "the throw's
  # error line DOES reach the creator ... fetch-handler.ts's console.error is
  # unambiguously on the path." The second clause is FALSE, established by
  # reading the control flow rather than by inferring from the empty count:
  #   sdks/bootstrap/src/fetch-handler.ts:216 is `await dispatch(...)` with NO
  #   try/catch around it. Every errResponse() call site -- 131, 141, 151, 162,
  #   174, 192 -- is BEFORE that line (wireId, input decode, method, module
  #   import, schema init). errResponse is logRawError's only caller.
  # So a creator handler throw propagates out of dispatch UNCAUGHT and is
  # handled in Rust (core/runtime.rs handler-threw arm). The rpc marker covers
  # FRAMEWORK-level failures only and CANNOT appear for a handler throw.
  #
  # Hence LOG_BOOM is expected to be 0 here and is printed as an observation,
  # not an assertion: the OR below is carried entirely by the app-text arm,
  # which is the load-bearing one. Do NOT read `rpc error marker x0` as a
  # symptom -- it misled this pilot for two ticks while the comment above
  # asserted the opposite. If LOG_BOOM ever goes non-zero, something routed a
  # handler throw through the framework rail and that is worth understanding.
  LOG_BOOM_CODE=$(curl -s -o /tmp/gp-boom-resp.json -w '%{http_code}' --max-time 15 \
    "http://localhost:$GATE_PORT/apps/$APP_NAME/__zeroship/v1/boom" \
    -H "X-Api-Key: $API_KEY" 2>/dev/null)
  command sleep 2
  curl -s -o /tmp/gp-logs-boom.json --max-time 20 "$LOG_URL" \
    -H "Authorization: Bearer $SC_PAT" 2>/dev/null || true
  LOG_BOOM=$(grep -oF -- "[zeroship:rpc] sanitized error" /tmp/gp-logs-boom.json 2>/dev/null | wc -l | tr -d ' ')
  LOG_BOOM_OWN=$(grep -oF -- "[starter] boom" /tmp/gp-logs-boom.json 2>/dev/null | wc -l | tr -d ' ')
  echo "  deployed throw probe: http=$LOG_BOOM_CODE, rpc error marker x$LOG_BOOM, app message x$LOG_BOOM_OWN"
  if [ "${LOG_BOOM_CODE:-200}" -lt 400 ] 2>/dev/null; then
    fail "the throw probe did NOT fail: boom returned http=$LOG_BOOM_CODE. A
      procedure whose body is \`throw\` answering 2xx is its own finding, and it
      means this arm measured nothing about error visibility. FAILED SETUP.
      Body: $(head -c 200 /tmp/gp-boom-resp.json)"
  elif [ "${LOG_BOOM:-0}" -ge 1 ] 2>/dev/null || [ "${LOG_BOOM_OWN:-0}" -ge 1 ] 2>/dev/null; then
    pass "deployed: a handler THROW reaches the creator's log surface (rpc marker x$LOG_BOOM, app text x$LOG_BOOM_OWN)"
  else
    fail "deployed: a procedure that THREW left NOTHING the creator can read.
      The isolate definitely ran -- boom's body is the throw -- and the same step
      proved seconds earlier that a console.log from the same app reaches this
      surface. So the error rail does not deliver, and a creator debugging their
      own failing procedure has no log to look at. See #333."
  fi

  # --- THE COMPARISON, which is the reason this step exists -----------------
  if [ "${LOG_DEV:-0}" -ge 1 ] 2>/dev/null && [ "${LOG_AFTER:-0}" -ge 1 ] 2>/dev/null; then
    pass "dev and deployed AGREE: the same operation's output is visible on both tiers"
  else
    fail "dev and deployed DIVERGE on log visibility -- dev=$LOG_DEV deployed=$LOG_AFTER.
      The same procedure ran on both tiers and only one of them let the creator
      see what it printed. Which tier is wrong is the question; that they differ
      is the finding."
  fi
fi

gp_close_step

# WHY THIS STEP EXISTS, and what every other auth assertion in this repo stops
# short of.
#
# `tests/e2e_auth_rpc.sh` proves the gateway turns a session cookie into a
# signed `ZeroShip-User` that reaches an `auth: user` procedure -- but the
# procedures it drives (whoami / whoamiStrict) only REPORT the identity. They
# never USE it. `tests/e2e_app_primitives_auth.sh` does drive a real two-user
# ownership scenario, but over the WORKER edge (`/dispatch/<app>`), and it
# BOOTS a gateway it then uses for exactly one health check.
#
# So "identity is delivered through the gateway" is covered, and "identity is
# ENFORCED on data" is covered a tier down, and the join of the two -- a
# procedure that scopes rows by owner, reached through the gateway -- was
# covered by nothing (#348). This step is that join.
#
# THE TWO EDGES AUTHENTICATE DIFFERENTLY, which is why this is not a re-point
# of the existing harness at a new URL: `/dispatch` takes an already-signed
# `ZeroShip-User` header, while the gateway takes a session COOKIE and mints
# that header itself. The minting is the part under test.
#
# FOUR CAUSES PRODUCE THE IDENTICAL SYMPTOM HERE -- every authed call 401s:
# a missing gateway signing key, a missing gateway DSN, a malformed `pws_`
# subject, and the wrong cookie name. Each was cleared separately before this
# was written, so a red below is an ownership finding rather than a stack
# problem. Two of the four are worth restating because they are counter-
# intuitive:
#   - COOKIE NAME is the DEV one, `zeroship_app_session`, UNPREFIXED. This file
#     exports ZEROSHIP_DEV_INSECURE=1 (line 81) and the gateway declares
#     `#[arg(long = "dev-insecure", env = "ZEROSHIP_DEV_INSECURE")]`
#     (crates/gateway/src/main.rs:165-166), so `insecure_dev` is TRUE even
#     though the launch line carries no flag, and oidc_rp.rs:980 selects
#     APP_SESSION_COOKIE_DEV. Reading the launch line alone gives the WRONG
#     answer here; `tests/e2e_auth_rpc.sh:157-159` already had it right.
#   - SUBJECT must satisfy is_pairwise_subject: "pws_" + EXACTLY 20 ascii
#     alphanumerics (PAIRWISE_SUB_BODY_LEN, crates/core/src/auth/mod.rs).
#     router/auth.rs rejects anything else by returning CookieOutcome::None,
#     which presents as anonymous -> 401, not as a parse error.
#
# DEV BASELINE this is diffed against (examples/auth-notes-db/scripts/smoke.sh
# against a live `pnpm dev`, 10/10 on the run that produced these):
#     notes.create (alice)        -> 200 with an id
#     notes.list   (alice)        -> 200, contains that id
#     notes.list   (bob)          -> 200, EMPTY
#     notes.get    (bob, alice's) -> 404, body carries no note text
#     notes.list   (anon)         -> 401
# Each assertion below names the dev result it is matching, so a divergence is
# readable without re-running the dev tier.
# =============================================================================
step 14 "Scoped data through the gateway: two identities, one row, no leak"

AN_ZSHIP="$ROOT/examples/auth-notes-db/dist/app.zship"
AN_APP="gpnotes"
AN_HOST="$AN_APP.localhost"
# Defined HERE rather than inherited: step 10 assigns GP_JOSE at :2254, but
# inside its own branch, so depending on it would make this step's outcome a
# function of whether an unrelated step ran.
AN_JOSE="$ROOT/node_modules/.pnpm/jose@6.2.3/node_modules/jose/dist/webapi/index.js"

# ARTIFACT FRESHNESS, asserted rather than assumed. This step provisions a
# PREBUILT .zship it does not build, so without this it measures whatever
# happens to be on disk. That matters most for the mutation this step exists to
# survive: dropping the owner_id predicate from notes.get and forgetting to
# rebuild makes the mutation never apply and the run print GREEN, which is
# indistinguishable from the platform being correct. Same class as the
# vite-plugin dist warning this harness prints at startup.
AN_STALE="$(find "$ROOT/examples/auth-notes-db/src" "$ROOT/examples/auth-notes-db/migrations" \
              "$ROOT/examples/auth-notes-db/vite.config.ts" -type f -newer "$AN_ZSHIP" 2>/dev/null | head -3)"

if [ ! -f "$AN_ZSHIP" ]; then
  fail "14: missing $AN_ZSHIP - run: pnpm --filter auth-notes-db build"
elif [ ! -f "$AN_JOSE" ]; then
  fail "14: missing jose at $AN_JOSE - cannot mint a session, so nothing below is testable"
elif [ -z "${SC_PAT:-}" ]; then
  # The app MUST be registered through the deploy API, not dev-provision: only
  # the deploy path calls ensure_app_client, so a dev-provisioned app has NO
  # row in zeroship.app_oauth_clients and every authed call 401s for that reason
  # alone (#328, and measured here on 2026-08-12 -- dev_provision.rs inserts
  # only into zeroship.users). SC_PAT already carries apps:write + apps:deploy.
  fail "14: SC_PAT is unset, so the app cannot be registered through the deploy API - a dev-provisioned app has no OAuth client and every assertion below would 401 for that reason"
else
  [ -z "$AN_STALE" ] \
    && pass "14: the auth-notes-db artifact is newer than its sources (the run measures the current app)" \
    || fail "14: STALE ARTIFACT - $AN_ZSHIP is older than $(echo "$AN_STALE" | tr '\n' ' ')
      Rebuild with: pnpm --filter auth-notes-db build
      Every verdict below would describe the PREVIOUS build, and a mutation of
      this example would silently not apply."

  AN_CREATE_JSON=$(curl -sf -X POST "http://localhost:$CONTROL_PORT/api/apps" \
    -H 'Content-Type: application/json' -H "Authorization: Bearer $SC_PAT" \
    -d "{\"name\":\"$AN_APP\"}" 2>/tmp/gp-notes-create.log)
  # No jq: this harness's CI image installs lsof, zstd and postgresql-client
  # and nothing else. Anchored on the QUOTED key so an error body with no id
  # yields empty and the fail arm fires.
  AN_APP_ID=$(printf '%s' "$AN_CREATE_JSON" | grep -oE '"id"[[:space:]]*:[[:space:]]*"[^"]+"' | head -1 | sed 's/.*"\([^"]*\)"$/\1/')
  if [ -z "$AN_APP_ID" ]; then
    fail "14: could not create the notes app: $(head -c 200 <<<"$AN_CREATE_JSON")"
  elif ! "$BIN/zeroship" deploy "$AN_ZSHIP" --app="$AN_APP_ID" \
         --control="http://localhost:$CONTROL_PORT" --token="$SC_PAT" \
         >/tmp/gp-notes-deploy.log 2>&1; then
    fail "14: deploy failed: $(tail -2 /tmp/gp-notes-deploy.log | tr '\n' ' ')"
  else
    pass "14: auth-notes-db registered through the deploy API ($AN_APP_ID)"

    # Schema through the REAL deployed path. The apply endpoint takes an IR
    # ENVELOPE, not an empty object: the first version of this step sent
    # `-d '{}'` and got `400 Json deserialize error: missing field 'kind'`.
    # The three working call sites (:1925, :2268, :2607) all --data-binary a
    # recorder-produced document, and all send the PAT.
    node --input-type=module - "$ROOT/sdks/vite-plugin/dist/gen-types/recorder.js" \
      "$ROOT/examples/auth-notes-db/migrations" >/tmp/gp-notes-ir.json 2>/tmp/gp-notes-ir.log <<'NODE'
import { pathToFileURL } from "node:url";
const [recorderPath, dir] = process.argv.slice(2);
const { discoverMigrations, recordMigration } = await import(pathToFileURL(recorderPath).href);
const migrations = await discoverMigrations(dir);
const documents = [];
for (const m of migrations) documents.push({ filename: m.stem + ".ir.json", body: await recordMigration(m.path) });
console.log(JSON.stringify({ kind: "ir", documents }));
NODE
    AN_APPLY=$(curl -s -o /tmp/gp-notes-apply.json -w '%{http_code}' -X POST \
      "http://localhost:$MIGRATED_PORT/v1/apps/$AN_APP_ID/migrations/apply" \
      -H 'Content-Type: application/json' -H "Authorization: Bearer $SC_PAT" \
      --data-binary @/tmp/gp-notes-ir.json)
    AN_APPLIED="$(node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const o=JSON.parse(s);process.stdout.write(String((o.applied||[]).length))}catch{process.stdout.write("0")}})' </tmp/gp-notes-apply.json)"
    { [ "$AN_APPLY" = "200" ] && [ "${AN_APPLIED:-0}" -ge 1 ] 2>/dev/null; } \
      && pass "14: notes migration applied through zeroship-migrated (applied=$AN_APPLIED ops)" \
      || fail "14: migrated could not apply (http=$AN_APPLY applied=${AN_APPLIED:-0}): $(head -c 200 /tmp/gp-notes-apply.json)"

    # The gateway needs route.oauth_client_id + sector_identifier or it answers
    # 503 client_not_provisioned. Control brokers the client at app-create; READ
    # it rather than inserting our own (e2e_auth_rpc lost that race and every
    # authed call failed with `app mismatch`).
    AN_OAC=""
    for _ in $(seq 1 20); do
      AN_OAC=$(docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -tAc \
        "select client_id from zeroship.app_oauth_clients where app_id = '$AN_APP_ID'" 2>/dev/null | tr -d '[:space:]')
      [ -n "$AN_OAC" ] && break
      sleep 1
    done
    if [ -z "$AN_OAC" ]; then
      fail "14: control never brokered an OAuth client for $AN_APP_ID - every authed call below would 401 for that reason alone"
    else
      pass "14: control brokered the app's OAuth client ($AN_OAC)"
      docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -q -c \
        "update zeroship.app_oauth_clients set sector_identifier = 'https://$AN_HOST'
           where app_id = '$AN_APP_ID' and (sector_identifier is null or sector_identifier = '')" >/dev/null 2>&1

      # Offline-mint one session cookie per identity, signed with the SAME key
      # the gateway loads (--signing-key-file). Bodies are exactly 20 chars.
      an_mint() {
        node --input-type=module -e '
import { readFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { importPKCS8, exportJWK, SignJWT } from "file://'"$AN_JOSE"'";
const [pem, app, sub, email] = process.argv.slice(1);
const key = await importPKCS8(readFileSync(pem,"utf8"), "EdDSA", { extractable:true });
const x = (await exportJWK(key)).x;
const kid = createHash("sha256").update(`{"crv":"Ed25519","kty":"OKP","x":"${x}"}`).digest("base64url");
const now = Math.floor(Date.now()/1000);
process.stdout.write(await new SignJWT({
  app, sub, email, email_verified: true, name: sub, avatar: null, scopes: [],
  auth_time: now, amr: ["pwd"],
}).setProtectedHeader({ alg:"EdDSA", typ:"zeroship-sess+jwt", kid })
  .setIssuer("https://api.zeroship.ai").setIssuedAt(now)
  .setExpirationTime(now + 3600).sign(key));
' "$GP_SIGNING_KEY" "$AN_OAC" "$1" "$2"
      }
      AN_ALICE=$(an_mint "pws_alice000000000000000" "alice@localhost")
      AN_BOB=$(an_mint "pws_bob00000000000000000" "bob@localhost")

      # an_rpc <cookie|-> <proc> [json] -> "<code> <body>"
      an_rpc() {
        local ck="$1" proc="$2" body="${3:-{\}}"
        local args=(-s -m 25 -o /tmp/gp-notes.body -w '%{http_code}' -H "Host: $AN_HOST"
                    -X POST -H 'content-type: application/json')
        [ "$ck" != "-" ] && args+=(-H "Cookie: zeroship_app_session=$ck" -H "Origin: http://$AN_HOST")
        local code; code=$(curl "${args[@]}" "http://localhost:$GATE_PORT/__zeroship/v1/$proc" -d "{\"json\":$body}")
        echo "$code $(cat /tmp/gp-notes.body)"
      }

      # PRECONDITION, not a result: if the authed path cannot even reach the
      # handler, every ownership verdict below is unproven. Assert it FIRST so
      # a stack problem cannot masquerade as a scoping finding.
      AN_READY=0
      for _ in $(seq 1 20); do
        [ "$(an_rpc "$AN_ALICE" notes.list | cut -d' ' -f1)" = "200" ] && { AN_READY=1; break; }
        sleep 1
      done
      [ "$AN_READY" = "1" ] \
        && pass "14: the gateway accepts a minted session (authed notes.list -> 200)" \
        || fail "14: gateway never accepted the session - ownership rows below are UNPROVEN"

      AN_ANON=$(an_rpc - notes.list)
      [ "$(echo "$AN_ANON" | cut -d' ' -f1)" = "401" ] \
        && pass "14: anon notes.list -> 401 (matches dev)" \
        || fail "14: anon notes.list -> $(echo "$AN_ANON" | cut -d' ' -f1), dev says 401"

      AN_CREATE=$(an_rpc "$AN_ALICE" notes.create '{"title":"Alice private","body":"for alice only"}')
      AN_NOTE_ID=$(echo "$AN_CREATE" | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
      { [ "$(echo "$AN_CREATE" | cut -d' ' -f1)" = "200" ] && [ -n "$AN_NOTE_ID" ]; } \
        && pass "14: alice notes.create -> 200 ($AN_NOTE_ID) (matches dev)" \
        || fail "14: alice notes.create -> $AN_CREATE, dev says 200 with an id"

      AN_ALIST=$(an_rpc "$AN_ALICE" notes.list)
      echo "$AN_ALIST" | grep -q "${AN_NOTE_ID:-__none__}" \
        && pass "14: alice's list contains her note (matches dev)" \
        || fail "14: alice's list is missing her own note: $AN_ALIST"

      # THE NEGATIVE. Status FIRST, then absence - an error response contains no
      # note id either, so asserting absence alone is how a broken tier passes.
      AN_BLIST=$(an_rpc "$AN_BOB" notes.list)
      [ "$(echo "$AN_BLIST" | cut -d' ' -f1)" = "200" ] \
        && pass "14: bob's list is a real 200, not an error that trivially omits it" \
        || fail "14: bob's list -> $(echo "$AN_BLIST" | cut -d' ' -f1), cannot judge scoping from it"
      echo "$AN_BLIST" | grep -q "${AN_NOTE_ID:-__none__}" \
        && fail "14: CROSS-USER LEAK - bob's list contains alice's note $AN_NOTE_ID" \
        || pass "14: bob's list does NOT contain alice's note (matches dev)"

      AN_BGET=$(an_rpc "$AN_BOB" notes.get "{\"id\":\"$AN_NOTE_ID\"}")
      [ "$(echo "$AN_BGET" | cut -d' ' -f1)" = "404" ] \
        && pass "14: bob reading alice's note BY ID -> 404 (matches dev)" \
        || fail "14: bob reading alice's note by id -> $AN_BGET, dev says 404"
      echo "$AN_BGET" | grep -q 'for alice only' \
        && fail "14: CONTENT LEAK - the refusal body carried alice's note text" \
        || pass "14: the refusal leaks no note content (matches dev)"
    fi
  fi
fi

gp_close_step

# --- 12. The creator DELETES the app, and everything it created goes away ---
#
# Rows 1-18 of docs/pilot/e2e-scenarios.md all cover an app being created,
# deployed, driven or enforced. NOTHING covered tearing one down, and nothing in
# tests/ has ever called `DELETE /api/apps/<id>` -- the only two `-X DELETE` in
# this tree hit Stripe's own API from e2e_stripe_connect_live.sh.
#
# `purge_app` (crates/control/src/api.rs) opens with "Tear down an app and all
# of its side-effecting state" and then enumerates exactly two steps: the
# manifest keyspace, and `registry.delete_app` (which DELETEs zeroship.apps +
# zeroship.oauth_clients in one txn and nothing else). The app's own Postgres
# schema -- the creator's tables and rows, created by zeroship-migrated -- is
# named in neither step. The teardown that WOULD remove it exists
# (crates/plugin-db/src/drop_namespace.rs, 7 steps, slot -> publication ->
# schema -> role) and has no production caller: its only callers are plugin-db's
# own integration tests.
#
# So this step asserts what a creator is entitled to assume, not what the code
# currently does. The schema assertion is EXPECTED RED at HEAD; the expected-
# failure block below carries it, so a run stays classifiable.
#
# THE VEHICLE IS scaffoldapp, deliberately, and it must be the LAST thing this
# file touches: it is the only app here that has a real per-app schema applied
# through the real zeroship-migrated (step 10c) AND an owner PAT that can
# authorize AppsDelete (SC_PAT, the app_members row written at step 10c).
step 12 "Teardown: the creator deletes the app, and its state goes with it"
if [ -z "${SC_APP_ID:-}" ] || [ -z "${SC_PAT:-}" ]; then
  fail "no scaffold app id or PAT, so the deletion path could not be exercised at all --
      this is a FAILED SETUP, not a passing teardown check"
else
  # --- BEFORE: prove the vehicle can discriminate ---------------------------
  # Without these three, every assertion after the DELETE would pass over an app
  # that was already gone, or a schema that never existed.
  DEL_ROW_PRE=$(docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -tAc \
    "select count(*) from zeroship.apps where id = '$SC_APP_ID'" 2>/dev/null | tr -d ' ')
  # LIKE, not '=', and that is a correction rather than a flourish. An app gets
  # TWO schemas -- `<app_id>` for the creator's tables and `<app_id>_migrations`
  # for the engine's journal. MEASURED on the golden database: 2 tables in the
  # first and 5 in the second. The '=' this replaced counted only the first, so
  # the failure below reported a residue of 2 when the real one is 7 across two
  # schemas.
  DEL_TBL_PRE=$(docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -tAc \
    "select count(*) from information_schema.tables where table_schema like '$SC_APP_ID%'" 2>/dev/null | tr -d ' ')
  DEL_SRV_PRE=$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 \
    "http://localhost:$GATE_PORT/apps/$SC_APP/" -H "X-Api-Key: $SC_API_KEY" 2>/dev/null)
  echo "  before delete: apps_row=$DEL_ROW_PRE per_app_tables=$DEL_TBL_PRE gateway=$DEL_SRV_PRE"

  if [ "${DEL_ROW_PRE:-0}" = "1" ] && [ "${DEL_TBL_PRE:-0}" -ge 1 ] 2>/dev/null; then
    pass "vehicle discriminates: the app row exists and its per-app schema holds $DEL_TBL_PRE table(s)"
  else
    fail "vehicle does NOT discriminate: apps_row=$DEL_ROW_PRE per_app_tables=$DEL_TBL_PRE.
      Every assertion below would pass over an app that was already gone or a
      schema that was never created. FAILED SETUP, not a pass. If this recurs,
      the per-app schema is no longer named by app_id -- check
      crates/plugin-db/src/drop_namespace.rs and the migrated apply path."
  fi

  # --- THE DELETE, over the real HTTP surface with the creator's own PAT -----
  DEL_BODY=$(curl -s -o /tmp/gp-delete.json -w '%{http_code}' --max-time 20 \
    -X DELETE "http://localhost:$CONTROL_PORT/api/apps/$SC_APP_ID" \
    -H "Authorization: Bearer $SC_PAT" 2>/dev/null)
  echo "  DELETE /api/apps/$SC_APP_ID -> $DEL_BODY $(head -c 120 /tmp/gp-delete.json)"
  if [ "$DEL_BODY" = "200" ] && grep -q '"deleted":true' /tmp/gp-delete.json 2>/dev/null; then
    pass "the creator's own PAT can delete the creator's own app (200 deleted:true)"
  else
    fail "DELETE /api/apps/<id> did not succeed for the app's OWNER (http=$DEL_BODY): $(head -c 200 /tmp/gp-delete.json)
      A creator who cannot delete their own app has no workaround, so this is a
      finding about the creator path, not about this harness."
  fi

  # --- AFTER: four things the creator is entitled to assume are gone --------
  DEL_ROW_POST=$(docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -tAc \
    "select count(*) from zeroship.apps where id = '$SC_APP_ID'" 2>/dev/null | tr -d ' ')
  [ "${DEL_ROW_POST:-1}" = "0" ] \
    && pass "the zeroship.apps row is gone" \
    || fail "the zeroship.apps row SURVIVED the delete (count=$DEL_ROW_POST)"

  # The gateway pulls routes from control on a 5s poll, so this is a WAIT, not a
  # single probe -- a one-shot check here would measure the poll interval rather
  # than the product. 30s is 6 polls.
  DEL_SRV_POST=""
  for _i in $(seq 1 30); do
    DEL_SRV_POST=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 \
      "http://localhost:$GATE_PORT/apps/$SC_APP/" -H "X-Api-Key: $SC_API_KEY" 2>/dev/null)
    [ "$DEL_SRV_POST" = "200" ] || break
    command sleep 1
  done
  if [ "$DEL_SRV_PRE" = "200" ] && [ "$DEL_SRV_POST" != "200" ]; then
    pass "the gateway stopped serving the deleted app (was $DEL_SRV_PRE, now $DEL_SRV_POST)"
  elif [ "$DEL_SRV_PRE" != "200" ]; then
    fail "the gateway was NOT serving this app before the delete (got $DEL_SRV_PRE), so
      'it stopped' proves nothing. FAILED SETUP, not a pass."
  else
    fail "the gateway is STILL serving the deleted app after 30s (6 route-sync polls):
      before=$DEL_SRV_PRE after=$DEL_SRV_POST"
  fi

  DEL_MAN=$(ls -1 "/tmp/gp-bundles/manifests/$SC_APP_ID" 2>/dev/null | wc -l | tr -d ' ')
  [ "${DEL_MAN:-1}" = "0" ] \
    && pass "the app's manifest keyspace is gone from the blob store" \
    || fail "manifests/$SC_APP_ID still holds $DEL_MAN object(s) after the delete"

  # THE ONE THAT IS RED AT HEAD. Deleting an app leaves the creator's own tables
  # and every row in them in Postgres forever, because no production code path
  # calls drop_namespace. See #330.
  #
  # READ THIS BEFORE CONCLUDING THAT A FIX FOR #331 DID NOT WORK. What this step
  # observes is the FIRST blocker in a chain of at least two, and it can only
  # ever see one at a time, because the cascade aborts the whole transaction on
  # the first trigger that raises. MEASURED against the live golden database by
  # walking the FK graph: exactly two DELETE-firing triggers are reachable from
  # a `DELETE FROM zeroship.apps` cascade, and NEITHER honours the
  # `zeroship.audit_retention` GUC (only 3 of the 17 append-only triggers in the
  # schema do -- app_audit, audit_events, authz_decisions):
  #   migrated_migration_audit_append_only   <- the one this run hits
  #   plan_change_events_immutable_trg       <- invisible here, 0 rows, because
  #                                             this app never changed plan
  # crates/control/src/proration.rs:188 INSERTs a plan_change_events row on
  # every plan change, so an app that has been on a paid plan carries them and
  # would hit the second the moment the first is cleared. Giving this step's app
  # a plan change before the delete would put both in play; that is the next
  # increment and it is NOT done here.
  DEL_TBL_POST=$(docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -tAc \
    "select count(*) from information_schema.tables where table_schema like '$SC_APP_ID%'" 2>/dev/null | tr -d ' ')
  DEL_NSP_POST=$(docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -tAc \
    "select count(*) from pg_namespace where nspname like '$SC_APP_ID%'" 2>/dev/null | tr -d ' ')
  if [ "${DEL_NSP_POST:-1}" = "0" ] && [ "${DEL_TBL_POST:-1}" = "0" ]; then
    pass "the app's per-app Postgres schema was dropped with the app"
  else
    fail "the per-app Postgres schema SURVIVED the delete -- schema(s)=$DEL_NSP_POST table(s)=$DEL_TBL_POST
      (was $DEL_TBL_PRE before). purge_app deletes the manifest keyspace and the
      zeroship.apps/oauth_clients rows and stops; nothing calls
      crates/plugin-db/src/drop_namespace.rs, whose only callers are plugin-db's
      own integration tests. The creator's data outlives the app. See #330.
      AND IT IS NOT MERELY THAT THE DELETE FAILED FIRST. MEASURED directly on
      the golden database, in a transaction that was rolled back: with both
      blocking triggers disabled the cascade RUNS TO COMPLETION (DELETE 1,
      apps 1 -> 0) and the schema count stays 2 and the table count stays 7.
      A DB cascade cannot drop a schema -- a schema is not a row - so fixing
      #331 will make the delete succeed and leave this residue exactly as it
      is. This assertion will still be the last one red."
  fi
fi

gp_close_step

# --- The verdict, and the two guards against a green run over nothing -------
#
# THE FLOOR IS A MEASUREMENT. Taken 2026-08-10 on this tree, running this script
# unmodified against the compose Postgres on :5440:
#
#     golden path: 24 passed, 0 failed        (exit 0)
#
# Per step: 1->2  2->4  3->1  4->2  5->1  6->2  7->7  8->2  9->3  = 24.
#
# CROSS-CHECKED against a second, independent instrument, because a count read
# out of the run it is meant to guard proves only that the run was self-
# consistent. `grep -c 'pass "'` counts the call sites in this file. Two of them
# (`created app`, `deployed real vite .zship`) live in step 3's ZEROSHIP_TOKEN
# arm and one (`dev-provisioned app`) in its else arm, so exactly one arm ever
# fires: static - 2 without a token, static - 1 with one. The two instruments
# disagree for different reasons if either is wrong, which is the whole point.
#
# THE COMMAND NAMED ABOVE MUST EXCLUDE COMMENTS, and for years it did not.
# `grep -c 'pass "'` matches this very block -- the sentence documenting the
# instrument contains the pattern it searches for. So the documented command has
# never returned the call-site count, and the gap grows every time someone
# writes about it. Measured 2026-08-10:
#
#     commit      raw   call sites   floor   floor correct?
#     c2adb8774    26      26        none    (no floor yet)
#     0c852e2e1    27      26          24    yes, 26 - 2
#     994bb679b    28      27          25    yes, 27 - 2
#     62695f077    29      28          26    yes, 28 - 2
#     08b0e09b2    30      29          26    one low, deliberately
#
# THE FLOOR WAS NEVER WRONG. Every value was derived from the true call-site
# count; only the prose quoted a number the named command does not produce. The
# one-low at HEAD is the deferred bump from 08b0e09b2, not drift.
#
# I MADE THE MISTAKE THIS INDUCES WHILE WRITING THIS COMMENT. Re-deriving with
# the documented command gave 30, and 30 - 2 = 28 against a floor of 26, so I
# drafted a paragraph reporting "two assertions of slack" in the gate. There is
# none. The contaminated instrument turns an accurate floor into an apparent
# defect, and the correction it suggests is to RAISE the floor -- which would
# have made the gate falsely red on an honest run. A self-counting instrument is
# worse than no second instrument, because it fails toward action.
#
# So the command to re-derive is, and the exclusion is not optional:
#
#     grep -v '^[[:space:]]*#' tests/golden_path.sh | grep -c 'pass "'
#
# The floor is then that count minus 2 (no token) or minus 1 (with one). At HEAD
# that is 29 - 2 = 27.
#
# ENUMERATED SECOND, the way e2e_dev_vs_deployed_env.sh does it, because a
# pattern count cannot see branching and this file has three kinds:
#
#   29  call sites outside comments
#   -2  step 3 is a three-site either/or: `created app` + `deployed real vite`
#       fire WITH a token, `dev-provisioned app` fires without. CI has no token,
#       so one of the three fires and two do not.
#   ------
#   27  green, no token   (28 with one)
#
# Checked by reading, not inferred, because these are what a grep would get
# wrong:
#   - the three `case` arms (deployed-fails-closed, camelCase insert, `with:`
#     eager-load) each have siblings that call `fail`, never `pass`, so each
#     contributes exactly one pass on a green run;
#   - step 9's three data-plane assertions sit behind `DB_UP`, which takes a
#     `fail` arm when the dev runtime never binds -- correct for a green-run
#     floor, and it means 27 is the GREEN total rather than a ceiling;
#   - no assertion of this file lives in tests/lib/e2e_stack.sh (unlike the env
#     harness, where seven do), so nothing is hiding outside the source.
#
# Two instruments, 27 and 27, and they fail differently: the pattern count goes
# wrong when prose contaminates it or sites move to a shared library; the
# enumeration goes wrong when someone misreads an arm.
#
# STILL NOT APPLIED. 26 stays. Both derivations are static, and neither has been
# confirmed by a four-service run since the last two assertions landed. CI runs
# this gate, so a floor that is one too HIGH turns main red on an honest run,
# while one too low only costs a single assertion of slack. The asymmetry decides
# it. The next clean run has only to confirm 27, not re-derive it.
#
# 24 IS THEREFORE THE LOWER OF THE TWO LEGITIMATE CONFIGURATIONS, which is where
# a floor has to sit - the same reasoning run_billing_suite.sh applies to its
# REDPANDA_BROKERS measurement. CI runs the no-token arm, so CI sits exactly on
# the floor.
#
# NO HEADROOM, deliberately, and this differs from the cargo-based gates. Their
# totals are DISCOVERED (feature resolution, optional deps, host capabilities)
# so a tight floor there fails honest runs. This total is not discovered: it is
# the number of pass()/fail() call sites this file reaches, fixed by the source.
# Every legitimate way to change it is an edit to this file, so a floor equal to
# the measurement costs nothing to raise when assertions are ADDED (25 >= 24
# passes untouched) and costs a deliberate edit when one is REMOVED. That
# asymmetry is the whole point.
#
# WHAT THE FLOOR DOES NOT CATCH: substitution. Deleting one assertion and adding
# an easier one keeps the total at 24. Nothing here can see that; review can.
# RE-MEASURED 2026-08-10, after step 10 (the scaffold leg) and step 8's worker
# restart landed. Full run against the compose Postgres on :5440, no token:
#
#     golden path: 43 passed, 6 failed        (exit 1 -- see below)
#     per step: 1->2  2->6  3->1  4->2  5->1  6->7  7->7  8->3  9->6  10->14 = 49
#
# THE SIX FAILURES ARE THE DELIVERABLE, NOT A BROKEN GATE. They are step 10's
# dev-vs-deployed comparisons: the scaffold template's six procedures answer 200
# under `pnpm dev` and 401 once deployed, because the template ships no RPC
# policy and the gateway's default is fail-closed. This gate is RED at HEAD on
# purpose and goes green with no edit to this file the moment the template
# declares a policy -- at which point the total is 49.
#
# THE FLOOR IS THE PASS COUNT, WHICH IS 43 TODAY. A floor is a lower bound on
# assertions that FIRED, so it takes the smaller of the two legitimate
# configurations, exactly as before.
#
# THE CALL-SITE CROSS-CHECK NO LONGER APPLIES UNMODIFIED, and saying so is the
# point of writing it down. `grep -c 'pass "'` outside comments returns 47,
# fewer than the 49 outcomes a green run produces, because step 10's comparison
# verdict is ONE call site inside a `while` loop that fires once per procedure.
# The old derivation (sites minus branch arms) silently assumed one site = one
# outcome; under a loop it under-counts, and a floor derived from it would sit
# below the real total and stop catching anything. Both numbers here are read
# off runs instead: 43 unmutated, 49 under MUTATE_SCAFFOLD_POLICY=1.
# RAISED 43 -> 46 on 2026-08-10 when step 11 (env.db id ordering, #236/#255)
# landed. Read off three full four-service runs, not derived:
#
#     run 1   46 passed, 8 failed     step 11: 5 outcome(s)
#     run 3   46 passed, 8 failed     step 11: 5 outcome(s)
#     run 2   47 passed, 7 failed     step 11: 5 outcome(s)   <- see below
#
# Steps 1-10 were byte-identical across all three (2,6,1,2,1,7,7,3,6,14), so the
# +3 is step 11's three green outcomes: db-todos builds, its migrations apply
# through zeroship-migrated, and the DEV tier returns byte order. The two reds
# are the deployed tier and the tier diff, and they are THE DELIVERABLE, exactly
# like step 10's six.
#
# RUN 2 IS WHY THE FLOOR IS 46 AND NOT 47, and it is worth more than the number.
# It scored one higher because its deployed population of 24 ids happened to be
# NON-discriminating: no pair of them sorts differently under bytes than under
# `en_US`, so the ordering check could not have failed, and the tier-diff line
# passed over a platform that two other runs proved wrong. The guard caught it
# and called it a failed setup - and the harness now tops the population up
# (ORD_ROUNDS) rather than accepting one. A gate that scores HIGHER when it
# measures LESS is the failure mode this floor exists to make visible.
#
# WHEN THE ENGINE GAINS A COLLATION SLOT (#255) the deployed verdict and the
# diff both go green with no edit here, and the total becomes 48 passed, 6
# failed (step 10's scaffold six). Raising the floor to 48 is that change's job.
#
# RAISED 46 -> 54 on 2026-08-10 when step 9 grew a deployed leg (docs/pilot/
# e2e-scenarios.md row 2 said "Deployed half not compared - see row 11", and
# row 11 never closed it either: its harness drives db-hitcounter, whose one
# column has no case boundary, so it is blind BY CONSTRUCTION to the naming
# seam this step exists to catch). Step 9 now builds db-todos, deploys it,
# applies its own migrations through zeroship-migrated, and diffs the SAME
# camelCase-insert and FK-eager-load calls against the dev results already
# captured -- 8 new outcomes (6->14), all green unmutated. Read off a clean
# four-service run:
#
#     before  golden path: 54 passed, 8 failed   step 9: 14 outcome(s)
#
# (the "before" total already includes the +8 -- "before" here means before
# the mutation below, not before this change landed; the pre-existing floor
# was measured at step 9 = 6 outcomes, so 46 + 8 = 54 is the same measurement
# taken twice, not two different numbers reconciled by arithmetic).
#
# MUTATION-PROVEN, not merely added. `sdks/bootstrap/src/runtime-entry.ts`
# was temporarily edited to force `naming: naming.snakeCase` on the DEPLOYED
# install path only (dev-entry.ts untouched) -- exactly the pre-#162 defect
# this scenario exists to catch, reintroduced on purpose. Rebuilt
# `@zeroship/bootstrap` and `zeroship-worker`, then re-ran:
#
#     mutated golden path: 50 passed, 12 failed   step 9: 14 outcome(s)
#
# The delta is exactly the four new comparison outcomes and nothing else:
# "deployed: insert with a camelCase field" and "deployed: with: eager-loaded"
# both failed with `{"message":"internal error",...}` (the column lookup for
# `userId` now misses, same as the original #162 report), and the two
# AGREE/DIVERGE comparison lines both flipped to DIVERGE (dev=ok,
# deployed=fail). Every other step's outcome count was unchanged
# (1,2,3,4,5,6,7,8,10,11 identical), and step 9's own three PRE-EXISTING dev
# assertions (seeded/insert/join) stayed green throughout, because the
# mutation touches only the deployed path. Reverted (clean `git diff` on
# runtime-entry.ts) and rebuilt again; the re-run matched the unmutated
# numbers above exactly, including the per-step outcome list.
# RAISED 54 -> 57 on 2026-08-10 for step 7d (the orphaned dev runtime, task
# #221). PROVENANCE OF THE +3, stated because it is weaker than the numbers
# above and a reader must not mistake it for a full-run measurement: 7d was run
# as a STANDALONE reproduction (7a's dev server plus the 7d block, lifted
# verbatim), because the full script needs a control plane, worker, gateway,
# migrated and Postgres that were not available on the machine that added it.
# That reproduction is a REAL dev server on the real example - it is the step's
# own environment, minus the deployed tier that 7d does not touch:
#
#     with the plugin setting ZEROSHIP_DIE_WITH_PARENT:  4 passed, 0 failed
#     with that one line mutated out and dist rebuilt:   2 passed, 2 failed
#
# (the 4 and 2 include 7a's own control, which is not new; the new outcomes are
# 3 and they are the delta.) The mutation was confirmed absent from BOTH
# `src/dev-server.ts` and the rebuilt `dist/dev-server.js` before the red run,
# because a mutation that never applied prints the same green as a surviving
# one. Under the mutation the orphan was pid 679607, still holding
# `examples/starter/.zeroship/kv.redb` 20 seconds after its vite was SIGKILLed.
# RAISED 57 -> 60 on 2026-08-11 for step 9b (dev-server fault classification,
# task #269), with the SAME provenance caveat as the +3 above: measured as a
# standalone reproduction of the 9b block alone (the full script still needs a
# stack this machine did not have), against the real examples/db-todos dev
# server, one variable being which build of sdks/vite-plugin/dist was in place:
#
#     with the classification fix built:   3 passed, 0 failed
#     with the two src files stashed
#       and dist rebuilt WITHOUT it:       1 passed, 2 failed
#
# The surviving pass in the red run is 9b's own CONTROL (writable artifacts ->
# gen-types succeeds, server stays up). It passing in BOTH runs is the point:
# it shows the red is the classification failing, not the app being broken.
#
# The arming was confirmed to APPLY before the red run was believed, and the
# first attempt did not: chmod 0500 on the directory alone left the write
# succeeding, because both artifacts already exist and `writeFile` over an
# existing file needs permission on the FILE. That run printed "did not exit"
# -- identical to a genuine failure to classify -- and would have been read as
# a red proof of a fix that had never been exercised.
# RAISED 60 -> 65 on 2026-08-11 for step 6c, which adds FIVE passes: a control
# that the runtime answers a raw socket at all, then the header byte cap accepted
# at exactly 16384 and refused at 16385, and the header COUNT accepted at 32 and
# refused at 33. Arithmetic is 60 + 5, measured on my own run, not derived.
# RAISED 65 -> 67 on 2026-08-11 for step 6c's DEPLOYED half: a control that the
# gateway answers a raw socket, and the tier-DIRECTION assertion (deployed must
# not bound headers at or below dev's 16 KiB). The deployed NUMBER is printed,
# never asserted -- it is a framework default, not our contract.
# RAISED 67 -> 70 on 2026-08-11 for step 2b, which adds THREE passes: the
# [secrets] overlay tier resolving control's DSN, the overlay demonstrably being
# read, and an explicit --db still beating it. Arithmetic is 67 + 3, and the
# three were measured on the pre-fix binary too: assertion A was RED there (that
# is the defect this step exists for) while B and C were green, so the step is
# discriminating, not merely present.
# RAISED 70 -> 71 on 2026-08-11 for step 2b's FOURTH assertion, added because the
# third one ("an explicit --db still beats the overlay") concluded from control
# NOT being healthy, which is equally what a slow start or an unrelated early
# config death looks like. The fourth requires the positive evidence -- control
# logged a connect failure, so it demonstrably reached the CLI DSN. Arithmetic is
# 70 + 1.
# RAISED 72 -> 74 on 2026-08-11 for the two workflow-scheduler-store assertions
# (#320): the migration-built columns and control's DML on them. Arithmetic is
# 72 + 2. Both were measured RED on a database carrying every other migration
# (cols 0 of 10, dml 0 of 8) and GREEN after the new migration alone, so they
# discriminate rather than merely count.
# RAISED 74 -> 75 on 2026-08-11 for the DDL-privilege guard. Arithmetic is 74 + 1.
# It is self-testing rather than merely green: MEASURED 3 as written, and 1 when
# the subject role is swapped for one that CAN do DDL, so it distinguishes the
# property from a probe that has stopped working.
# RAISED 85 -> 87 on 2026-08-11 for step 12 (teardown). Arithmetic is 85 + 2, and
# 2 is the whole point: step 12 emits SIX outcomes and only two of them pass at
# HEAD. The four reds are #331 and are carried in GOLDEN_EXPECTED_FAILURES below.
# MEASURED on this tree: 87 passed / 12 failed.
# RAISED 89 -> 101 on 2026-08-12 for step 14 (scoped data through the gateway).
# Arithmetic is 89 + 12, where 12 is step 14's ENTIRE outcome count: unlike step
# 12, every one of them passes at HEAD, so this step adds nothing to
# GOLDEN_EXPECTED_FAILURES. MEASURED on this tree: 101 passed / 16 failed.
#
# The step is PROVEN load-bearing, not merely green, and the proof is the
# asymmetry rather than the redness. Dropping `owner_id` from the notes.get
# filter in examples/auth-notes-db/src/index.ts and REBUILDING the artifact (the
# freshness guard at the top of the step refuses a stale one, which is what makes
# this mutation honest) gives 99 passed / 18 failed. EXACTLY TWO rows flip -- the
# by-id read, which returns 200 where dev says 404, and the content-leak check,
# whose body then carries alice's note text to bob. The other TEN stay green,
# INCLUDING bob's list. That is the point: the mutation models scoping that is
# still correct on the list path and absent on the by-id path, which is precisely
# the shape the step header names as easy to write and easy to miss. A test that
# went entirely red here would not have distinguished it from a broken stack.
#
# LEDGER GAP, recorded so the chain above is not read as complete: three earlier
# raises (71 -> 72, 75 -> 85, 87 -> 89) have no entry. The arithmetic in the
# entries that DO exist is still checkable, but it does not compose end to end.
GOLDEN_MIN_PASSED="${GOLDEN_MIN_PASSED:-101}"

# Guard 2: every DECLARED step must have run and asserted something. See the
# reasoning beside GP_EXPECTED_STEPS at the top of this file.
gp_silent=0
for _s in $GP_EXPECTED_STEPS; do
  _n="${GP_STEP_OUTCOMES[$_s]-MISSING}"
  if [ "$_n" = "MISSING" ]; then
    echo "  ✗ step $_s never ran - it was declared in GP_EXPECTED_STEPS and produced no banner"
    gp_silent=$((gp_silent + 1))
  elif [ "$_n" -eq 0 ]; then
    echo "  ✗ step $_s ran but asserted NOTHING - every check inside it was skipped in silence"
    gp_silent=$((gp_silent + 1))
  fi
done

# Guard 2b: the SYMMETRIC check - a step that ran but was never declared.
# Without this, guard 2 only protects the ids someone remembered to list, and a
# new step is unguarded from the moment it is written until someone notices. It
# is not hypothetical: step 9b shipped undeclared and stayed that way, holding
# 14 of 75 outcomes outside the guard. Adding 9b to the list fixes that one
# step; this loop is what stops the next one, because the failure mode is
# forgetting the list exists, and a check that depends on remembering is the
# thing being forgotten. The report below prints declared ids only, so an
# undeclared step is invisible there too - which is why this names it.
for _s in "${!GP_STEP_OUTCOMES[@]}"; do
  case " $GP_EXPECTED_STEPS " in
    *" $_s "*) ;;
    *)
      echo "  ✗ step $_s ran and produced ${GP_STEP_OUTCOMES[$_s]} outcome(s) but is NOT in"
      echo "    GP_EXPECTED_STEPS, so nothing checks whether it asserted anything and it"
      echo "    is absent from the summary. Add it to the list."
      gp_silent=$((gp_silent + 1))
      ;;
  esac
done

echo ""
echo "============================================"
echo "  golden path: $PASS passed, $FAIL failed (floor $GOLDEN_MIN_PASSED)"
for _s in $GP_EXPECTED_STEPS; do
  printf '    step %s: %s outcome(s)\n' "$_s" "${GP_STEP_OUTCOMES[$_s]-MISSING}"
done
echo "  MUTATION: MUTATE_DEV_DIVERGE=1 must turn step 6 RED"
# Stated as a DELTA and a cause, not an absolute pair. "(49 passed, 0 failed)"
# stood here until 2026-08-11 and was unreachable by then: it was measured when
# step 10's six were the only failures, and step 11's two collation reds (#255)
# landed afterwards and are untouched by a scaffold policy. Someone running the
# control to check this gate is load-bearing got a mismatch and had to decide
# whether the gate or the platform was wrong. An absolute total rots on every
# step that lands; +6 and "only step 11 survives" does not.
echo "  MUTATION: MUTATE_SCAFFOLD_POLICY=1 must clear step 10's SIX comparisons --"
echo "            passed rises by exactly 6, and the only failures left are step"
echo "            11's collation reds while #255 is open (measured 2026-08-11:"
echo "            67/8 unmutated -> 73/2 mutated)."
if [ "$FAIL" -gt 0 ] && [ "${MUTATE_SCAFFOLD_POLICY:-0}" != "1" ]; then
  echo "  NOTE: step 10's six scaffold comparisons are RED AT HEAD BY DESIGN -- the template"
  echo "        ships no RPC policy, so its procedures answer 200 in dev and 401 deployed."
  echo "        That is the defect, not a broken gate. See docs/pilot/e2e-scenarios.md."
  echo "  NOTE: step 11's two reds are RED AT HEAD BY DESIGN too -- the deployed \`id\` column is"
  echo "        created by zeroship-migrated and inherits the database collation, so ORDER BY id"
  echo "        is not creation order there. Blocked on the engine (#255); see the mail"
  echo "        ZEROSHIP-2026-08-10-189 in ~/.claude/inter-projects.md."
fi
echo "============================================"

# Printed on SUCCESS as well as failure: a number nobody sees until the gate has
# already failed cannot warn anyone.
rc=0
[ "$FAIL" -eq 0 ] || rc=1

# WHICH failures, not just how many.
#
# This harness is RED AT HEAD by design, so `rc` carries no information about
# regressions: it is already 1 before anything new breaks. A ninth failure moves
# one digit in one summary line. That is the same shape as a gate that cannot
# tell a filtered-green from a real one, and it is why the identities are
# recorded above.
#
# Each pattern below is a substring of a failure label that is red BY DESIGN and
# attributed to an open ticket. The check is TWO-SIDED on purpose:
#   - a failure matching NO pattern is a REGRESSION, and is what this exists for
#   - a pattern matching NO failure means the defect was FIXED and the list was
#     not updated, which is a bookkeeping error the same way an unexplained drop
#     below GOLDEN_MIN_PASSED is
# Both set rc=1. The second will fire the day #260, #255 or #331 lands, and
# updating this list belongs in that same change - exactly as lowering the floor
# does.
#
# NOT ALL FOUR CATEGORIES ARE "BY DESIGN". #260 and #255 are decisions waiting on
# an operator. Step 12's four are a KNOWN DEFECT (#331) that this harness found:
# an app that has ever applied a migration cannot be deleted at all, because
# `migrated_migration_audit.app_id -> zeroship.apps` is ON DELETE CASCADE and
# that table carries a BEFORE DELETE append-only trigger, so the cascade aborts
# the whole transaction. They are listed here for the same reason as the others
# -- so a NEW failure is still visible -- and not because anyone chose them.
GOLDEN_EXPECTED_FAILURES=${GOLDEN_EXPECTED_FAILURES:-"scaffold notes.list|scaffold notes.add|scaffold notes.delete|scaffold files.upload|scaffold files.list|scaffold visits.bump|sort({id:-1}) is NOT creation order|DIVERGE on id ordering|DELETE /api/apps/<id> did not succeed|zeroship.apps row SURVIVED the delete|gateway is STILL serving the deleted app|per-app Postgres schema SURVIVED the delete|dev: step 6 drove getMessages on the dev tier|dev and deployed DIVERGE on log visibility|left the creator no log line"}
IFS='|' read -r -a _pats <<< "$GOLDEN_EXPECTED_FAILURES"
# FIXED-STRING matching, both directions, and this is not stylistic. The first
# draft joined the patterns into one ERE, and one of them - `sort({id:-1}) is
# NOT creation order` - is not a valid ERE: `{id:-1}` is a malformed interval,
# so that pattern silently matched nothing and its own known failure was
# reported as UNEXPECTED. Caught by running the classifier over a synthetic set
# of the known 8 before wiring it, where the correct answer is 0 unexpected and
# it said 1.
gp_expected_hit() {
  local _p
  for _p in "${_pats[@]}"; do printf '%s' "$1" | grep -qF "$_p" && return 0; done
  return 1
}
gp_unexpected=0
for _f in "${GP_FAILURES[@]}"; do
  gp_expected_hit "$_f" || {
    gp_unexpected=$((gp_unexpected+1))
    echo "  UNEXPECTED FAILURE (not in the red-at-HEAD set): $_f" >&2
  }
done
gp_stale=0
for _p in "${_pats[@]}"; do
  printf '%s\n' "${GP_FAILURES[@]}" | grep -qF "$_p" || {
    gp_stale=$((gp_stale+1))
    echo "  STALE EXPECTATION (declared red-at-HEAD, did not fail): $_p" >&2
  }
done
echo "  failures: $FAIL total, $((FAIL - gp_unexpected)) expected, $gp_unexpected unexpected, $gp_stale stale expectation(s)"
if [ "$gp_unexpected" -gt 0 ]; then
  echo "FAIL: $gp_unexpected failure(s) are NOT in the documented red-at-HEAD set." >&2
  echo "      This gate is red at HEAD by design, so the exit status alone could not" >&2
  echo "      have told you that. Each line above is a regression or a new defect." >&2
  rc=1
fi
if [ "$gp_stale" -gt 0 ]; then
  echo "FAIL: $gp_stale declared red-at-HEAD failure(s) did not occur." >&2
  echo "      Either the defect was fixed and GOLDEN_EXPECTED_FAILURES was not updated" >&2
  echo "      in the same change, or the assertion stopped running. Both need saying;" >&2
  echo "      do not delete the pattern without checking which one it was." >&2
  rc=1
fi
if [ "$gp_silent" -gt 0 ]; then
  # Wording covers BOTH guard-2 arms and guard 2b. Saying "declared step(s)"
  # here was wrong for the undeclared-step case - the complaint there is
  # precisely that it was NOT declared - and a summary line that contradicts
  # the detail above it is how a reader learns to skip the summary.
  echo "FAIL: $gp_silent step accounting problem(s) above: a step that asserted" >&2
  echo "      nothing, a declared step that never ran, or a step that ran without" >&2
  echo "      being declared. Each is indistinguishable from a step that passed," >&2
  echo "      and this script would otherwise have exited 0 over it." >&2
  rc=1
fi
if [ "$PASS" -lt "$GOLDEN_MIN_PASSED" ]; then
  echo "FAIL: only $PASS assertions passed, fewer than the $GOLDEN_MIN_PASSED this gate expects." >&2
  echo "      Assertions do not vanish by accident: either a check stopped firing" >&2
  echo "      (step 4's asset probe is the one guarded by a conditional) or one was" >&2
  echo "      removed. If the removal was deliberate, lower GOLDEN_MIN_PASSED in the" >&2
  echo "      same change and say why; do not treat the gap as slack." >&2
  rc=1
fi
exit "$rc"
