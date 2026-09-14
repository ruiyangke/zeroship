# shellcheck shell=bash
# ============================================================================
# tests/lib/e2e_stack.sh — shared E2E bring-up for the zeroship platform.
#
# Single source of truth for the full local stack the gateway-level E2E
# harnesses need:
#
#   stack_workspace $WORK scratch dir + PIDFILE + $DBURL + the ed25519 key the
#                   harness OP signs with (also the gateway's signing key) + the
#                   harness JWKS endpoint + the gateway broker master secret.
#   stack_pg_up     ephemeral Postgres (docker) + the FULL platform migration
#                   set from db/migrations-ts applied from scratch via the
#                   `zeroship-platform-migrate` bin (Platform profile).
#   stack_up        stack_workspace + stack_pg_up, then control + worker +
#                   gateway booted with generated service credentials and
#                   health-polled. Non-blocking: binaries run in the background;
#                   the function returns once all three are health-green.
#                   A harness whose topology differs (e.g. e2e_platform.sh's
#                   three workers) calls stack_workspace + stack_pg_up +
#                   mint_creator_bearer and starts its own binaries.
#   mint_creator_bearer
#                   seeds a creator principal (a `users` row) and mints a
#                   platform OAuth access token for it from the harness OP.
#                   Exports $ADMIN_TOKEN.
#   deploy_zship    create an app named <slug> via the control API and deploy a
#                   prebuilt .zship with `zeroship deploy --token=$ADMIN_TOKEN`;
#                   echoes the created app id on stdout (return 0), or returns 1.
#   stack_down      kill the binary PIDs, docker rm -f the PG container, rm $WORK.
#
# This file is SOURCED, not executed. It assumes `set -uo pipefail` in the
# caller and that the caller defines pass()/fail() helpers IF it wants the
# bring-up to emit ✓/✗ lines (stack_up calls them when present; otherwise it
# falls back to plain echo). Errors during bring-up return non-zero so the
# caller can decide whether to abort.
#
# Tunables (export BEFORE calling stack_up):
#   ZEROSHIP_CONTROL_PORT ZEROSHIP_WORKER_PORT ZEROSHIP_GATEWAY_PORT PG_PORT
#   E2E_PLATFORM_OP_PORT         - listen ports
#   PG_CONTAINER                 - docker container name
#   ZEROSHIP_WORKER_THREADS      - worker --threads
#   E2E_ROOT                     - repo root (auto-derived)
#
# THE PORT DEFAULTS BELOW ARE A FALLBACK, NOT A DESIGN. A per-harness constant lets
# a harness run back-to-back with a DIFFERENT harness; it does nothing for the
# case that actually happens on a shared box - two agents running the SAME
# harness, or two harnesses whose bands were chosen independently and overlap
# (9181 is both tests/e2e_stripe_billing.sh's and
# tests/e2e_real_app_end_to_end.sh's; 9393/8393/8303 is both
# tests/e2e_dev_vs_deployed_db.sh's and tests/e2e_redeploy_replaces_app.sh's,
# and those two run in the SAME CI job). A caller that wants isolation calls
# `zs_ports_reserve` (tests/lib/e2e_ports.sh) BEFORE sourcing this file; the
# `:=` defaults then leave its allocation alone, and `stack_up`'s port-freeing
# step skips a port that was allocated rather than pinned.
#
# The harness OP already works this way and has since it was written: it binds
# port 0 and reads the number back from a file (tests/lib/runtime_secrets.sh
# e2e_platform_op_up). Nothing about a listen port here is different.
#
# Exports after stack_up: ZEROSHIP_CONTROL_PORT ZEROSHIP_WORKER_PORT
#   ZEROSHIP_GATEWAY_PORT PG_CONTAINER WORK
#   PIDFILE (newline-separated binary PIDs under $WORK), DBURL.
# ============================================================================

# --- repo root + binary dir (derive once) ----------------------------------
if [ -z "${E2E_ROOT:-}" ]; then
  # this file lives at <root>/tests/lib/e2e_stack.sh
  E2E_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fi
E2E_BIN="$E2E_ROOT/target/release"
E2E_JOSE_JS="$E2E_ROOT/node_modules/.pnpm/jose@6.2.3/node_modules/jose/dist/webapi/index.js"
# shellcheck source=tests/lib/runtime_secrets.sh
source "$E2E_ROOT/tests/lib/runtime_secrets.sh"

# --- defaults (private/non-colliding band; override before stack_up) --------
: "${ZEROSHIP_CONTROL_PORT:=9120}"
: "${ZEROSHIP_WORKER_PORT:=8098}"
: "${ZEROSHIP_GATEWAY_PORT:=8012}"
: "${PG_PORT:=5454}"
: "${PG_CONTAINER:=zs-e2e-stack-pg}"
: "${ZEROSHIP_WORKER_THREADS:=2}"

# --- emit helpers: prefer caller-provided pass/fail, else plain echo --------
_stk_ok()   { if declare -F pass >/dev/null 2>&1; then pass "$1"; else echo "  ✓ $1"; fi; }
_stk_bad()  { if declare -F fail >/dev/null 2>&1; then fail "$1"; else echo "  ✗ $1"; fi; }

# --- run accounting: four outcomes, not one ---------------------------------
# These harnesses funnelled four incompatible meanings into a single `known()`
# counter that does not fail at the default STRICT=0 (audited 2026-08-10, task
# #256): a real open defect, a missing prerequisite, behaviour that is
# explicitly NOT a defect, and a branch not reached this run. A counter that
# means all four cannot be read, and one of the four is not a soft failure at
# all -- it is the absence of evidence.
#
#   pass     the check ran and held
#   fail     the check ran and did not hold
#   known    a real, open defect (name a ticket)
#   skipped  the check DID NOT RUN
#
# `skipped` is the one that matters. A harness that could not run a check has
# produced NO evidence about it, so a run carrying skips must not exit 0 --
# otherwise a fresh checkout with no built `dist/*.zship` exits green having
# exercised nothing. Set ALLOW_SKIP=1 to opt out deliberately, which is a
# choice someone has to type rather than the default.
e2e_skipped() {
  SKIPPED=$(( ${SKIPPED:-0} + 1 ))
  echo "  ⊘ SKIPPED (did not run): $1"
}

# Print the four counts and return the run's verdict. Callers: `e2e_verdict`
# as the last statement, or `e2e_verdict || exit 1`.
e2e_verdict() {
  local p="${PASS:-0}" f="${FAIL:-0}" k="${KNOWN:-0}" s="${SKIPPED:-0}"
  echo ""
  echo "============================================"
  echo "  Results: $p passed, $f failed, $k known-fail, $s skipped"
  echo "============================================"
  if [ "$f" -gt 0 ]; then
    return 1
  fi
  if [ "$s" -gt 0 ] && [ "${ALLOW_SKIP:-0}" != "1" ]; then
    echo "  FAIL: $s check(s) never ran, so this run is not evidence about them." >&2
    echo "        Build the missing artifacts, or set ALLOW_SKIP=1 to accept the gap." >&2
    return 1
  fi
  return 0
}

# node helper: read a JSON field from stdin (e.g. `... | _stk_jget '.id'`)
_stk_jget() { node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{try{const o=JSON.parse(s);process.stdout.write(String(o$1??'')+'\n')}catch(e){console.log('')}})"; }

# --- preflight: required binaries + tooling ---------------------------------
stack_preflight() {
  local b
  for b in zeroship zeroship-control zeroship-gate zeroship-worker zeroship-data-cdc-server; do
    [ -x "$E2E_BIN/$b" ] || { _stk_bad "missing $E2E_BIN/$b - run cargo build --release"; return 2; }
  done
  [ -f "$E2E_ROOT/packages/zero-migrate-cli/dist/cli-bin.js" ] || { _stk_bad "missing the zero-migrate CLI - run: pnpm install && pnpm build"; return 2; }
  [ -f "$E2E_JOSE_JS" ] || { _stk_bad "missing jose at $E2E_JOSE_JS"; return 2; }
  command -v docker  >/dev/null || { _stk_bad "docker required"; return 2; }
  command -v openssl >/dev/null || { _stk_bad "openssl required"; return 2; }
  command -v node    >/dev/null || { _stk_bad "node required"; return 2; }
  return 0
}

# --- stack_workspace: $WORK dir + signing key + broker secret + $DBURL ------
#
# Split out of stack_up so a harness with a NON-standard topology (e.g.
# tests/e2e_platform.sh, which runs THREE workers to exercise CHWBL routing
# and cross-worker isolation) can reuse the workspace + PG + credential halves
# without inheriting stack_up's single-worker bring-up.
stack_workspace() {
  WORK="$(mktemp -d -t zs-e2e-stack-XXXXXX)"
  mkdir -p "$WORK/blobs" "$WORK/blob-cache"
  PIDFILE="$WORK/pids"
  : > "$PIDFILE"
  DBURL="postgres://postgres:zeroship@localhost:$PG_PORT/zeroship"

  # Per-service database DSNs. `--db` is gone from every binary: a DSN admits
  # userinfo, so it is secret-classed and secrets never travel through argv.
  e2e_export_database_urls "$DBURL" || return 1

  # One ed25519 key with two consumers: the harness's own platform OP publishes
  # it as its JWKS and signs admin bearers with it, and the gateway loads it as
  # its wrapper-token signing key. Control no longer takes a signing key at all
  # -- it verifies bearers against the ISSUER's published key, which is why the
  # harness has to serve one.
  openssl genpkey -algorithm ed25519 -out "$WORK/signing-key.pem" 2>/dev/null
  chmod 600 "$WORK/signing-key.pem"

  # Broker master secret. cd54028e7 ("cut end-user login to the platform OP via
  # per-app brokered oac_ clients") made the gateway REFUSE TO START without it,
  # and this file was not updated -- so `stack_up` has been unable to bring a
  # gateway up since, taking every harness that sources it with it (e2e_auth_rpc,
  # e2e_uri, e2e_s3_*, e2e_app_primitives_render, tests/e2e_browser).
  #
  # An earlier version of this comment claimed every harness bringing its own
  # stack passed the flag directly and was unaffected. That was wrong, and the
  # claim is why nobody rechecked: several own-stack harnesses launch a gateway
  # with no secret at all (e2e_app_primitives{,_auth,_kv_storage}, e2e_platform,
  # bench_platform; m0_gate and supabase_deploy_e2e were others until they were
  # deleted with their subjects). Running e2e_app_primitives.sh
  # is what surfaced it - it dies at "gateway unhealthy" before reaching a single
  # env.db assertion, so its header's "env.db GREEN" describes a run that has not
  # happened since cd54028e7. The three app_primitives ones now generate their
  # own secret; e2e_platform now takes it from here.
  openssl rand -base64 48 > "$WORK/gate-secret"
  chmod 600 "$WORK/gate-secret"

  # Stand the harness OP up BEFORE `e2e_export_runtime_secrets`: it exports
  # ZEROSHIP_AUTH_PLATFORM_ISSUER, and that helper only fills the name in when
  # it is unset. Before control starts, too -- control reads the issuer once at
  # boot. Its PID lands in $PIDFILE (created above), so `stack_down` reaps it.
  e2e_platform_op_up "$WORK/signing-key.pem" "$WORK" || return 1

  # Keep the explicit signing/broker files above because the gateway consumes
  # them directly. Generate every other mandatory service input and export it
  # through the binaries' normal CLI/env configuration surface.
  ZEROSHIP_GATEWAY_SIGNING_KEY_FILE="$WORK/signing-key.pem"
  ZEROSHIP_GATEWAY_BROKER_SECRET_FILE="$WORK/gate-secret"
  e2e_export_runtime_secrets "$WORK" || return 1

  # --- the worker-enrolment envelope ---------------------------------------
  #
  # Control derives a worker instance's advertised address from the OBSERVED
  # peer socket of the enrolment connection and validates it against these two
  # settings. Both halves must be declared or `EnrolmentEnvelope::is_declared`
  # is false and control answers every enrolment `envelope_unset` with a 503:
  # absence refuses, it does not default open. Until this block existed, no
  # file in this repository declared one, so enrolment could not succeed
  # anywhere.
  #
  # LOOPBACK IS ADMITTED BECAUSE IT IS DECLARED, NOT BY DEFAULT. The network
  # comparison in `derive_address`
  # (crates/zeroship-control/src/worker_enrolment.rs) is the only thing that
  # admits a loopback peer, and an undeclared envelope still refuses one.
  # Everything this file starts runs on localhost, so `127.0.0.0/8` is what the
  # peer socket reports and `127.0.0.0/8` is what is stated here.
  #
  # THE PORT HALF IS DERIVED FROM THE WORKER'S OWN PORT, not written out. A
  # literal would drift the first time a harness moved the worker, and control
  # would then refuse the enrolment as `port_outside_envelope` while every
  # health probe in this file stayed green. A harness whose topology runs
  # workers on ports other than $ZEROSHIP_WORKER_PORT - tests/e2e_platform.sh
  # runs three - has to state its own range by exporting this before calling
  # here; the `:-` keeps that override.
  ZEROSHIP_CONTROL_WORKER_ENROLMENT_NETWORKS="${ZEROSHIP_CONTROL_WORKER_ENROLMENT_NETWORKS:-127.0.0.0/8}"
  ZEROSHIP_CONTROL_WORKER_ENROLMENT_PORTS="${ZEROSHIP_CONTROL_WORKER_ENROLMENT_PORTS:-$ZEROSHIP_WORKER_PORT}"
  export ZEROSHIP_CONTROL_WORKER_ENROLMENT_NETWORKS ZEROSHIP_CONTROL_WORKER_ENROLMENT_PORTS

  export WORK PIDFILE DBURL PG_CONTAINER
  return 0
}

# --- stack_pg_up: ephemeral Postgres + the FULL platform migration set ------
# Requires $WORK (for the migrate log) — call stack_workspace first.
stack_pg_up() {
  # --- ephemeral Postgres ---------------------------------------------------
  docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
  # The relay requires logical WAL and finite retention so an abandoned slot
  # cannot retain WAL indefinitely. Match the platform Compose posture.
  docker run --name "$PG_CONTAINER" -d -p "$PG_PORT:5432" \
    -e POSTGRES_PASSWORD=zeroship -e POSTGRES_USER=postgres -e POSTGRES_DB=zeroship \
    postgres:16 -c wal_level=logical -c max_connections=300 -c max_slot_wal_keep_size=8GB >/dev/null \
    || { _stk_bad "docker run postgres failed"; return 1; }
  # Readiness = three CONSECUTIVE successful queries, not one pg_isready.
  # Measured 2026-08-09 by sampling both probes ~30x/s against a fresh
  # postgres:16: there is a window in which `pg_isready` reports ready and
  # `psql -c 'select 1'` still fails (the entrypoint's initdb-phase server is
  # torn down and restarted). A single pg_isready let a run through that
  # window and the migration step then died with
  # "connect: error communicating with the server" — the harness reporting a
  # broken database rather than a slow one.
  local i ready=0
  for i in $(seq 1 90); do
    if docker exec "$PG_CONTAINER" psql -U postgres -d zeroship -tAc 'select 1' >/dev/null 2>&1; then
      ready=$((ready + 1))
      [ "$ready" -ge 3 ] && break
    else
      ready=0
    fi
    sleep 1
  done
  if [ "$ready" -ge 3 ]; then
    _stk_ok "ephemeral PG ready on :$PG_PORT"
  else
    _stk_bad "PG never became query-able on :$PG_PORT"
    docker logs --tail 20 "$PG_CONTAINER" 2>&1 | sed 's/^/      /'
    return 1
  fi

  if [ -f "$E2E_ROOT/deploy/ops/postgres-init.sql" ]; then
    local init_out
    if init_out="$(docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 \
        < "$E2E_ROOT/deploy/ops/postgres-init.sql" 2>&1)"; then
      _stk_ok "applied deploy/ops/postgres-init.sql"
    else
      # Was mute: the previous form sent both streams to /dev/null, so a
      # failure here printed "postgres-init.sql failed" and nothing else.
      _stk_bad "postgres-init.sql failed"
      printf '%s\n' "$init_out" | tail -10 | sed 's/^/      /'
    fi
  fi

  local mig_log="$WORK/migrate.log"
  if zs_platform_migrate "postgres://postgres:zeroship@localhost:$PG_PORT/zeroship" \
      --migrations-dir "$E2E_ROOT/db/migrations-ts" \
      --project-schema zeroship --project-id zeroship > "$mig_log" 2>&1; then
    _stk_ok "platform migrations applied cleanly from scratch (zeroship-platform-migrate)"
  else
    _stk_bad "zeroship-platform-migrate FAILED (see $mig_log)"; tail -20 "$mig_log"; return 1
  fi
  return 0
}

# --- stack_up: PG + migrations + control/worker/gateway, non-blocking -------
stack_up() {
  stack_preflight || return $?
  stack_workspace || return 1
  export ZEROSHIP_CONTROL_PORT ZEROSHIP_WORKER_PORT ZEROSHIP_GATEWAY_PORT
  stack_pg_up || return 1

  # --- free the ports -------------------------------------------------------
  # ONLY the ports this run did NOT allocate. `lsof -ti :$p | xargs kill -9`
  # exists because a harness that pins a CONSTANT has to reclaim that constant
  # from its own leaked previous run - and it cannot tell that corpse from a
  # peer agent's live control plane fifteen minutes into its own run. MEASURED
  # 2026-08-21: an unrelated server on :9181 was SIGKILLed by a single run of
  # tests/e2e_stripe_billing.sh, which carried the same line.
  #
  # A port that came from `zs_ports_reserve` (tests/lib/e2e_ports.sh) was
  # verified free at the moment it was claimed and is held for the life of the
  # run, so anything listening on it is by definition a stranger. Skipping it
  # here is not a special case - it is the whole reason to allocate.
  local i p
  for p in $ZEROSHIP_CONTROL_PORT $ZEROSHIP_WORKER_PORT $ZEROSHIP_GATEWAY_PORT; do
    case " ${ZS_PORTS_HELD:-} " in *" $p "*) continue ;; esac
    lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
  done

  # --- control --------------------------------------------------------------
  # Every credential arrives through the canonical environment names exported by
  # `stack_workspace`; only non-secret operational values are on the line.
  "$E2E_BIN/zeroship-control" --port "$ZEROSHIP_CONTROL_PORT" \
    --blob-store "$WORK/blobs" \
    > "$WORK/control.log" 2>&1 &
  echo $! >> "$PIDFILE"
  for i in $(seq 1 30); do curl -sf "http://localhost:$ZEROSHIP_CONTROL_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done
  curl -sf "http://localhost:$ZEROSHIP_CONTROL_PORT/readyz" >/dev/null 2>&1 \
    && _stk_ok "control healthy" || { _stk_bad "control unhealthy"; tail -20 "$WORK/control.log"; return 1; }

  e2e_start_cdc_relay "$E2E_BIN/zeroship-data-cdc-server" || return 1
  # --- worker ---------------------------------------------------------------
  "$E2E_BIN/zeroship-worker" --port "$ZEROSHIP_WORKER_PORT" --threads "$ZEROSHIP_WORKER_THREADS" \
    --control-url "http://localhost:$ZEROSHIP_CONTROL_PORT" \
    --blob-store "$WORK/blobs" --poll-interval 2 > "$WORK/worker.log" 2>&1 &
  echo $! >> "$PIDFILE"
  for i in $(seq 1 30); do curl -sf "http://localhost:$ZEROSHIP_WORKER_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done
  curl -sf "http://localhost:$ZEROSHIP_WORKER_PORT/readyz" >/dev/null 2>&1 \
    && _stk_ok "worker healthy" || { _stk_bad "worker unhealthy"; tail -20 "$WORK/worker.log"; return 1; }

  # --- gateway --------------------------------------------------------------
  "$E2E_BIN/zeroship-gate" --port "$ZEROSHIP_GATEWAY_PORT" --control-url "http://localhost:$ZEROSHIP_CONTROL_PORT" \
    --worker-urls "http://localhost:$ZEROSHIP_WORKER_PORT" --blob-store "$WORK/blobs" \
    --blob-cache-disk-root "$WORK/blob-cache" --poll-interval 2 \
    --signing-key-file "$WORK/signing-key.pem" \
    --broker-secret-file "$WORK/gate-secret" \
    > "$WORK/gate.log" 2>&1 &
  echo $! >> "$PIDFILE"
  for i in $(seq 1 30); do curl -sf "http://localhost:$ZEROSHIP_GATEWAY_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done
  curl -sf "http://localhost:$ZEROSHIP_GATEWAY_PORT/readyz" >/dev/null 2>&1 \
    && _stk_ok "gateway healthy" || { _stk_bad "gateway unhealthy"; tail -20 "$WORK/gate.log"; return 1; }

  return 0
}

# --- mint_creator_bearer: creator OAuth access token, exports $ADMIN_TOKEN
#
# Two halves, and both are load-bearing:
#
#   the principal   control resolves the token's `sub` against `zeroship.users`
#                   and refuses a missing or lifecycle-disabled row. There is no
#                   platform role to seed any more: the staff roles and their
#                   universal-allow policy are deleted, so this principal's
#                   authority is the self-service baseline plus the ORGANIZATION
#                   seat it gains by CREATING the apps the harness then drives:
#                   the first `POST /api/apps` on a principal with no
#                   organization mints their personal one and seats them as its
#                   owner, and every app they then create lives in its default
#                   project.
#   the token       an `at+jwt` from the harness OP. `scope` becomes the token
#                   policy, which is intersected with the owner's authority, so
#                   this is the ceiling on what the harness may do.
#
# The scope list is one scope string per Cedar action. Billing is NOT in it; a
# harness that needs `billing:*` passes its own list as the first argument.
#
# `organization:create` IS LOAD-BEARING AND IS NOT SPARE BREADTH. The token scope
# is a CEILING, so it must carry the authority the personal-organization mint
# described above needs: `create_app` with no `project_id` requires
# `organization:create` at `Resource::Any` BEFORE it mints anything, so without
# this the very first `POST /api/apps` is a 403 and every later step of every
# harness that creates an app is unreachable.
#
# It was missing from 2026-09-06 to 2026-09-08, and the shape of that omission is
# worth keeping: the SAME commit that made the scope mandatory also edited this
# line (to drop `deployments:rollback`) and wrote the paragraph above describing
# the mint -- so the prose documenting the flow and the list forbidding it landed
# together. A comment describing a mechanism is not evidence the mechanism is
# reachable. Five harnesses were red on it.
mint_creator_bearer() {
  local scope="${1:-organization:create apps:read apps:write apps:deploy apps:archive deployments:read env:read env:write secrets:read secrets:write}"
  local owner pg_database
  pg_database="${E2E_PG_DATABASE:-zeroship}"
  owner="$(node <<'NODE'
const { randomBytes } = require("node:crypto");
const bytes = randomBytes(16);
let milliseconds = BigInt(Date.now());
for (let index = 5; index >= 0; index -= 1) {
  bytes[index] = Number(milliseconds & 0xffn);
  milliseconds >>= 8n;
}
bytes[6] = (bytes[6] & 0x0f) | 0x70;
bytes[8] = (bytes[8] & 0x3f) | 0x80;
const body = BigInt(`0x${bytes.toString("hex")}`).toString(36).padStart(25, "0");
process.stdout.write(`usr_${body}`);
NODE
)"
  docker exec -i "$PG_CONTAINER" psql -U postgres -d "$pg_database" -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.users (id, email, name, email_verified_at)
VALUES ('$owner', 'e2e-$owner@zeroship.test'::citext, 'E2E Stack Creator', NOW());
SQL
  ADMIN_TOKEN="$(e2e_mint_platform_bearer "$owner" "$scope")"
  ADMIN_SUBJECT="$owner"
  export ADMIN_TOKEN ADMIN_SUBJECT
  if [ "$(echo -n "$ADMIN_TOKEN" | awk -F. '{print NF}')" = "3" ]; then
    _stk_ok "minted platform creator bearer (sub=$owner)"
    return 0
  else
    _stk_bad "admin bearer mint failed: $ADMIN_TOKEN"
    return 1
  fi
}

# --- deploy_zship <slug> <path-to-.zship>: create app + deploy, echo app id -
deploy_zship() {
  local slug="$1" zship="$2"
  local j id dep
  j="$(curl -s -X POST "http://localhost:$ZEROSHIP_CONTROL_PORT/api/apps" \
        -H 'Content-Type: application/json' -H "Authorization: Bearer $ADMIN_TOKEN" \
        -d "{\"name\":\"$slug\"}")"
  id="$(echo "$j" | _stk_jget '.id')"
  if [ -z "$id" ]; then echo "    create-app($slug) failed: $j" >&2; return 1; fi
  dep="$("$E2E_BIN/zeroship" deploy "$zship" --app="$id" --control="http://localhost:$ZEROSHIP_CONTROL_PORT" --token="$ADMIN_TOKEN" 2>&1)"
  if ! echo "$dep" | grep -q "deploy_hash"; then echo "    deploy($slug) failed: $dep" >&2; return 1; fi
  echo "$id"
  return 0
}

# --- dev-server readiness: a deadline, and a diagnosis instead of a guess ---
#
# Every dev-vs-deployed harness used to wait with a fixed iteration count sized
# on an idle machine (`for _ in $(seq 1 25); do probe && break; sleep 2; done`)
# and report one string when it ran out: "dev app never came up". That string
# is a conclusion, and it has been wrong in both directions we have measured:
#
#   cold dep cache   storage harness, 2026-08-11: run 1 RED, run 2 green, same
#                    code. vite was "ready in 806 ms" and the runtime had
#                    registered all four plugins; what expired was the 40 s
#                    budget while vite logged "Re-optimizing dependencies
#                    because lockfile has changed" twice.
#   host contention  the CI file's own note on the workflows harness: 3/3 green
#                    idle, 3/3 RED on 12 busy cores, one mode being "the
#                    runtime missed the 25 x 2s readiness window". Its comment
#                    concludes "every one is a timing budget", and that is the
#                    stated reason two harnesses are NOT wired into CI.
#
# So the budget is expressed in SECONDS and is overridable per host, and the
# timeout message reports which failure mode the LOG shows rather than naming
# the app by default. A bigger fixed count would only move the cliff.
#
# ZS_DEV_READY_TIMEOUT: seconds to wait (default 180). The default is chosen to
# cover a cold dependency optimisation on a loaded host; it is a ceiling, not a
# cost, because the loop exits on the first successful probe.
#
# stack_dev_diagnosis <logfile> -- prints WHY the wait failed, from evidence.
# Pure text in, text out: no services, no timing, so it is unit-testable and is
# unit-tested (tests/lib/dev_ready_selftest.sh).
stack_dev_diagnosis() {
  local log="${1:-}"
  if [ ! -s "${log:-/nonexistent}" ]; then
    echo "the dev process produced no output at all -- it failed to launch, or its log went elsewhere"
    return 0
  fi
  # Order matters: the most specific cause that can explain a silent app wins,
  # and each arm names the layer to look at next.
  if grep -qiE 'blocked port|network error: blocked' "$log"; then
    local bp
    bp="$(grep -oiE 'blocked port [0-9]+' "$log" | head -1)"
    echo "the runtime refused to fetch modules over a blocked port (${bp:-see log}) -- the WHATWG bad-ports list is enforced by our own fetch (crates/zeroship-runtime/src/web/fetch/bad_ports.rs); pick a different port, this is not an app fault"
    return 0
  fi
  if grep -qiE 'is already in use|address already in use|port is already allocated' "$log"; then
    echo "the dev server's port was already in use, so it never bound -- another harness or a leftover process holds it; this is not an app fault"
    return 0
  fi
  if grep -qiE 'Re-optimizing dependencies|Optimizing dependencies|new dependencies optimized' "$log"; then
    if grep -qiE 'ready in ' "$log"; then
      echo "vite was re-optimising dependencies (cold cache after a lockfile change) and reached ready, but the app did not answer inside the deadline -- raise ZS_DEV_READY_TIMEOUT before suspecting the app"
    else
      echo "vite was re-optimising dependencies (cold cache after a lockfile change) and never reported ready inside the deadline -- raise ZS_DEV_READY_TIMEOUT before suspecting the app"
    fi
    return 0
  fi
  if grep -qiE 'ready in ' "$log"; then
    echo "vite reported ready and no dependency work was pending, but the app never answered the probe -- this one IS a candidate app or runtime fault, read the log below"
    return 0
  fi
  echo "vite never reported ready and no recognised cause is in the log -- read the tail below; the classifier has no rule for this shape"
  return 0
}

# stack_wait_dev <label> <logfile> <probe command...>
# Polls the probe until it exits 0 or the deadline passes. On timeout it prints
# the diagnosis and the log tail, then returns 1 -- the CALLER decides whether
# that is a `fail` + exit, because harnesses differ in what they do next.
# On success it reports the elapsed seconds when the wait was slow, so a run
# that only just made it is visible rather than silently green.
stack_wait_dev() {
  local label="$1" log="$2"; shift 2
  local deadline="${ZS_DEV_READY_TIMEOUT:-180}"
  local waited=0
  while [ "$waited" -lt "$deadline" ]; do
    if "$@" >/dev/null 2>&1; then
      [ "$waited" -ge 30 ] && echo "  note: $label became ready after ${waited}s (deadline ${deadline}s)"
      return 0
    fi
    sleep 2
    waited=$((waited+2))
  done
  echo "  $label did not become ready within ${deadline}s."
  echo "  diagnosis: $(stack_dev_diagnosis "$log")"
  echo "  --- last 20 lines of $log ---"
  tail -20 "$log" 2>/dev/null || echo "  (log unreadable)"
  return 1
}

# --- stack_down: kill PIDs, remove PG container, clean WORK -----------------
stack_down() {
  if [ -n "${PIDFILE:-}" ] && [ -f "$PIDFILE" ]; then
    local pid
    while read -r pid; do [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; done < "$PIDFILE"
  fi
  wait 2>/dev/null || true
  docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
  [ -n "${WORK:-}" ] && rm -rf "$WORK"
  return 0
}
