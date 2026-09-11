#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Dev vs deployed: run ONE identical operation sequence against `pnpm dev` and
# against the same app deployed behind the gateway, then (a) diff the RESULTS
# and (b) assert ABSOLUTE properties of the DEPLOYED answers.
#
# This is the only test in the repo that compares the two sides. Every other
# e2e script proves one side answers; none proves both answer the SAME, and
# that gap is where this project's seam bugs have lived (LocalFs dropping
# contentType while S3 kept it; the SQLite dev tier diverging from Postgres).
#
# What it caught on first run (2026-08-09, docs/pilot/e2e-scenarios.md):
#   - examples/kv-dashboard had no src/server/config.ts, so all 16 procedures
#     resolved to `auth: user` and the gateway refused every one. The example
#     was green in dev and completely unreachable deployed.
#   - `kv.keys.list` ORDER differs by backend: redb iterates sorted, Redis SCAN
#     does not, and docs/reference/kv.md specifies neither.
#
# WHY SECTION 5 EXISTS (added 2026-08-09). The diff answers "do the two tiers
# AGREE"; it structurally cannot answer "is the answer RIGHT". If redb and
# Redis were broken the SAME way -- a memo that never memoises, a rate limiter
# that never refuses, a TTL stored at the wrong magnitude -- the two sides
# produce byte-identical rows and this script reports success. That is not
# hypothetical: production shipped `Error.stack` to anonymous callers for weeks
# while `e2e_dev_vs_deployed_auth.sh` stayed green, because dev leaked the same
# stack (fixed in 39f6aa350; docs/pilot/e2e-scenarios.md, "What a dev-vs-
# deployed comparison cannot see").
#
# The SCRUBS make it worse here, and they are load-bearing for the diff, so
# they stay -- section 4 asserts on a RAW, UNSCRUBBED capture instead. Each
# scrub below erases exactly the evidence for one contract:
#   "nonce"/"builtAt"   the memoised value's IDENTITY. `getOrSet` promises the
#                       second call returns the FIRST call's value; scrubbed,
#                       a memo that recomputes every time is invisible.
#   "price"             same for the quote cache.
#   "ttlMs"/"resetMs"   the TTL MAGNITUDE. A 60s TTL stored as 60ms scrubs to
#                       the same `<V>` on both sides.
#   "leaseId"           which lease is held after a contended acquire.
#
# MUTATIONS -- each edits examples/kv-dashboard/src/index.ts, which BOTH tiers
# build from, so the relative diff stays GREEN and only the absolute verdicts
# move. That is the demonstration, not just the control:
#   MUTATE=rate-never-limits  `allowed: true` -- rate6 must flip
#   MUTATE=cache-recompute    memo/cache keys get a random suffix -- the five
#                             hit/identity verdicts must flip
#   MUTATE=ttl-clamped        every ttlOption returns 1s -- the two TTL bands
#                             must flip
#   MUTATE=delete-noop        the string delete stops deleting -- strDel and
#                             the key-absence verdict must flip
#
# MEASURED 2026-08-09 against a 44-pass baseline: rate-never-limits 43/1,
# ttl-clamped 42/2, delete-noop 41/3, and in all three the DIFF STAYED GREEN --
# both tiers wrong, comparison satisfied, absolute verdict red. cache-recompute
# is the one exception, 38/6: its mechanism is a random key SUFFIX, and the key
# names are listed in the `keys` row, so that row diverges too. The five
# verdicts it exists to move still moved; the diff going red is collateral from
# the mutation, not from the defect it models.
#
# Prereqs (docs/runbooks/local-dev.md):
#   cargo build --release -p zeroship-control -p zeroship-worker -p zeroship-gateway -p zeroship-cli --bins
#   pnpm install && pnpm build
#   docker (Postgres on :5440 as compose-postgres-1, plus an ephemeral Redis)
#   pnpm install in examples/kv-dashboard
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release"
# shellcheck source=tests/lib/runtime_secrets.sh
source "$ROOT/tests/lib/runtime_secrets.sh"
APP="$ROOT/examples/kv-dashboard"
ZSHIP="$APP/dist/app.zship"
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
zs_scratch_db_resolve zeroship_devdeploy || exit $?
PG_DB="$TEST_DB"
DB_URL="${DATABASE_URL:-postgres://postgres:zeroship@localhost:5440/$PG_DB}"
# zs_scratch_db_cleanup reaches the server through `run_psql`; without it the
# generated database is leaked and the library says so rather than pretending.
run_psql() { docker exec "$PG_CONTAINER" psql -U "$PG_USER" "$@"; }
# Distinct from golden_path.sh (9390/8390/8300) so both can run at once.
ZEROSHIP_CONTROL_PORT="${ZEROSHIP_CONTROL_PORT:-9392}"
ZEROSHIP_WORKER_PORT="${ZEROSHIP_WORKER_PORT:-8392}"
ZEROSHIP_GATEWAY_PORT="${ZEROSHIP_GATEWAY_PORT:-8302}"
DEV_PORT="${DEV_PORT:-3011}"
# VITE's own port. DEV_PORT above is the RUNTIME port -- what `zeroship serve`
# binds, and the only one the app's vite.config names. Left undeclared until
# 2026-08-11, vite silently took its :5173 GLOBAL default: nothing here knew
# that port, so cleanup could not free it and vite outlived every run orphaned
# at PPID 1. Measured on the sibling workflows harness before its identical fix
# (590aeab85) -- one such process, 38 minutes old, still holding 127.0.0.1:5173.
# :5173 is also every vite's default, so two harnesses at once fought over it
# (#173's class). Private, and --strictPort at the call so a conflict fails
# loudly rather than moving to a port nobody watches. See #272.
VITE_PORT="${VITE_PORT:-5011}"
REDIS_PORT="${REDIS_PORT:-6396}"
REDIS_CONTAINER="zs-devdeploy-redis"
ZEROSHIP_CONTROL_KEY="dd-ck"; ZEROSHIP_CONTROL_MASTER_KEY="dd-mk"
export E2E_STALE_WORKER_BEARER="${E2E_STALE_WORKER_BEARER:-devdeploy-worker-key-0123456789abcd}"
APP_NAME="kvdash"
MUTATE="${MUTATE:-none}"

PASS=0; FAIL=0; PIDS=()
pass() { PASS=$((PASS+1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }
cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  # BOTH ports, by LISTENER not by recorded PID: `( cd x && vite )&` records the
  # subshell, and vite outlives it as an orphan. Freeing DEV_PORT alone kills
  # `zeroship serve` and leaves vite holding its own port forever.
  for _p in "$DEV_PORT" "$VITE_PORT"; do
    lsof -ti :"$_p" 2>/dev/null | xargs -r kill 2>/dev/null || true
  done
  docker rm -f "$REDIS_CONTAINER" >/dev/null 2>&1 || true
  # The mutations edit a tracked source file. Restore it whatever happens,
  # including on the exit paths that abort mid-run.
  [ -f "${MUTATE_BAK:-}" ] && cp "$MUTATE_BAK" "$APP/src/index.ts"
  # Drops the per-run database, ABOVE the KEEP_WORK return: KEEP_WORK keeps the
  # work dir, and letting it also keep a database nobody named would leak one
  # per run under a name no later run reuses. To keep the data, name it -
  # TEST_DB=<name> is never dropped.
  zs_scratch_db_cleanup
  [ "${KEEP_WORK:-0}" = "1" ] && { echo "  work dir kept: $WORK"; return; }
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
  "crates/zeroship-kv/src crates/zeroship-kv-v8/src crates/zeroship-runtime/src crates/zeroship-worker/src crates/zeroship-gateway/src crates/zeroship-control/src sdks/kv/src" \
  "zeroship zeroship-worker zeroship-gate zeroship-control dev-provision" \
  || { _zs_fresh_rc=$?; [ "$_zs_fresh_rc" -ne 0 ] && exit "$_zs_fresh_rc"; }

echo "=== dev vs deployed (kv-dashboard) ==="
echo "  mutation: $MUTATE"

# --- the probe: one deterministic sequence, printed as RESULTS ---
# TWO outputs come off the same calls:
#   stdout    the CONTRACT row, volatile fields blanked, which feeds the diff.
#             Anything NOT blanked here is asserted identical across redb and
#             Redis.
#   $RAWFILE  the body VERBATIM, one line per label, which feeds the absolute
#             verdicts in section 4. The scrubs are exactly what makes those
#             contracts invisible to the diff, so section 4 must not read the
#             scrubbed text.
probe() {
  local url="$1" rpc="$1/__zeroship/v1"
  raw_call() {
    curl -sS -m 20 -X POST -H 'content-type: application/json' \
      "$rpc/$1" -d "{\"json\":${2:-{\}}}" 2>&1
  }
  scrub() {
    sed -E \
      -e 's/"(resetMs|ttlMs|expiresAt|generatedAt|builtAt|createdAt|acquiredAt)":[0-9-]+/"\1":<V>/g' \
      -e 's/"(nonce|token|leaseId)":"[^"]*"/"\1":"<V>"/g' \
      -e 's/"price":[0-9.]+/"price":<V>/g' \
      -e 's/kv-demo:session:[a-z0-9-]+/kv-demo:session:<V>/g' \
      -e 's/(kv-demo:rate:[a-z0-9-]+):[0-9]+/\1:<WINDOW>/g'
      # ^ the rate key ends in `Math.floor(now / RATE_WINDOW_MS)`. The two
      #   sides are probed minutes apart, so the window number is a CLOCK, not
      #   a backend property, and comparing it would report the passage of a
      #   minute as a dev-vs-deployed divergence.
  }
  # row <label> <proc> [json-payload]
  row() {
    local label="$1" body
    body="$(raw_call "$2" "${3:-}")"
    printf '%s %s\n' "$label" "$body" >> "$RAWFILE"
    printf '%-8s %s\n' "$label" "$(printf '%s' "$body" | scrub)"
  }
  raw_call kv.clear >/dev/null                   # start from a known state
  row visit1 kv.visit
  row visit2 kv.visit
  for i in 1 2 3 4 5 6; do row "rate$i" kv.rate.hit '{"actor":"probe"}'; done
  row cache1 kv.cache.quote '{"sku":"probe-sku"}'
  row cache2 kv.cache.quote '{"sku":"probe-sku"}'
  row memo1 kv.memo.get '{"label":"probe-memo"}'
  row memo2 kv.memo.get '{"label":"probe-memo"}'
  row lease1 kv.lease.acquire '{"owner":"owner-a"}'
  row lease2 kv.lease.acquire '{"owner":"owner-b"}'
  row leaseC kv.lease.clear
  row strSet kv.string.set '{"value":"hello probe","ttlMs":60000}'
  row strExp kv.string.expire '{"ttlMs":120000}'
  row strPer kv.string.persist
  row strDel kv.string.delete
  # keys.list is compared as a SET, not a sequence: redb iterates sorted and
  # Redis SCAN does not, and docs/reference/kv.md specifies neither order, so
  # asserting sequence would fail on an unspecified property rather than a
  # defect. The SET still has to match, which is the part the contract implies.
  # When kv.md states an order, delete the sort and compare verbatim -- see
  # docs/pilot/e2e-scenarios.md, scenario 11 KV leg.
  #
  # The set is built from the `keys` ARRAY, not by splitting the whole line on
  # commas. The old version did the latter, which does not canonicalise
  # anything: the first member carries the `{"keys":[` prefix and the last
  # carries the `]`, so two backends listing the same set in different orders
  # produce different fragments and sort to different text. It was never
  # noticed because the row below was returning an EMPTY list.
  #
  # The prefix is EMPTY, not "kv-demo:". `kv.keys.list` lists through
  # `kv.namespace("kv-demo:")` (examples/kv-dashboard/src/index.ts), so a
  # "kv-demo:" argument searches for `kv-demo:kv-demo:*` and matches nothing.
  # This row asked for that until 2026-08-09 and returned `{"keys":[]}` on both
  # tiers for every run: the diff compared empty to empty and reported success,
  # and it was the ABSOLUTE verdict below ("lists the visit counter") that said
  # so on its first run. A row that cannot fail is the same blind spot this
  # section exists for, one level up.
  local keys_body
  keys_body="$(raw_call kv.keys.list '{"prefix":"","limit":50}')"
  printf 'keys %s\n' "$keys_body" >> "$RAWFILE"
  printf '%-8s members=%s cursor=%s\n' keys \
    "$(printf '%s' "$keys_body" | scrub \
        | grep -oE '"keys":\[[^]]*\]' | sed -E 's/"keys":\[|\]//g' \
        | tr ',' '\n' | sed '/^$/d' | sort | tr '\n' ' ')" \
    "$(printf '%s' "$keys_body" | grep -qE '"cursor":null' && echo null || echo present)"
  # The cursor is compared as null-or-not rather than by value: it is
  # documented opaque, and redb and Redis mint different tokens, so its BYTES
  # are not a contract. Whether the page ended IS.
}

# --- 1. real build ---
#
# The mutations edit the app source BOTH tiers build from. That is deliberate:
# a mutation applied to one side only would show up in the diff and prove
# nothing about the blind spot, which is precisely "both sides wrong the same
# way". Each one must leave the diff GREEN and move exactly the section-4
# verdicts named beside it.
if [ "$MUTATE" != "none" ]; then
  MUTATE_BAK="$WORK/index.ts.bak"
  cp "$APP/src/index.ts" "$MUTATE_BAK"
  case "$MUTATE" in
    rate-never-limits)
      # The limiter still counts; it just never refuses. Only rate6 changes.
      sed -i 's/allowed: count <= RATE_LIMIT,/allowed: true,/' "$APP/src/index.ts"
      grep -q 'allowed: true,' "$APP/src/index.ts" || { fail "mutation did not apply"; exit 1; }
      ;;
    cache-recompute)
      # Every read mints a fresh key, so `getOrSet`/the quote cache can never
      # hit. Values stay well-formed; only their IDENTITY across two calls
      # changes -- which is what the nonce/price/builtAt scrubs erase.
      sed -i -e 's|`memo:${safeLabel}`|`memo:${safeLabel}:${Math.random()}`|' \
             -e 's|`cache:quote:${safeSku}`|`cache:quote:${safeSku}:${Math.random()}`|' \
             "$APP/src/index.ts"
      grep -q 'Math.random()}`' "$APP/src/index.ts" || { fail "mutation did not apply"; exit 1; }
      ;;
    ttl-clamped)
      # Every explicit ttlMs collapses to 1s. Both tiers store the wrong
      # magnitude, and the `ttlMs` scrub blanks the evidence.
      sed -i 's/Math.min(Math.max(1_000, Math.trunc(ttlMs)), 86_400_000)/1_000/' "$APP/src/index.ts"
      grep -q 'const safe = 1_000;' "$APP/src/index.ts" || { fail "mutation did not apply"; exit 1; }
      ;;
    delete-noop)
      # The string delete reports success and deletes nothing.
      sed -i 's/const deleted = must(await store().delete(TEXT_KEY));/const deleted = { deleted: true };/' \
        "$APP/src/index.ts"
      grep -q 'const deleted = { deleted: true };' "$APP/src/index.ts" \
        || { fail "mutation did not apply"; exit 1; }
      ;;
    *) fail "unknown MUTATE=$MUTATE"; exit 2 ;;
  esac
  echo "  MUTATED ($MUTATE): both tiers build from the edited source"
fi
( cd "$APP" && pnpm build ) > "$WORK/build.log" 2>&1
[ -f "$ZSHIP" ] && pass "built app.zship ($(du -k "$ZSHIP" | cut -f1)KB)" \
  || { fail "build produced no app.zship"; tail -20 "$WORK/build.log"; exit 1; }

# Every RPC resource must declare an auth posture. Without it the procedure
# resolves to `auth: user` and the gateway refuses it -- green in dev, 401
# deployed. That is exactly how kv-dashboard shipped.
d="$WORK/unpack"; mkdir -p "$d"; tar -xf "$ZSHIP" -C "$d"
total=$(grep -oE '"rpc:[^"]+":' "$d/manifest.json" | wc -l)
authed=$(grep -oE '"rpc:[^"]+":\{[^}]*"auth":' "$d/manifest.json" | wc -l)
[ "$total" -gt 0 ] && [ "$authed" -eq "$total" ] \
  && pass "all $total rpc resources declare an auth posture" \
  || fail "only $authed of $total rpc resources declare auth (missing src/server/config.ts?)"

# --- 2. dev side ---
for _p in "$DEV_PORT" "$VITE_PORT"; do
  lsof -ti :"$_p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
done
( cd "$APP" && ./node_modules/.bin/vite --port "$VITE_PORT" --strictPort > "$WORK/dev.log" 2>&1 ) & PIDS+=($!)
# Readiness: a deadline plus a log-derived diagnosis, not a fixed 20 x 2s count
# sized on an idle machine (#273). Sourced HERE and not at the top: e2e_stack.sh
# opens with `: "${ZEROSHIP_CONTROL_PORT:=9120}"` and four more of that shape, which only
# assign when unset, so sourcing it above this harness's own port block would
# hand it the library's ports.
#
# The probe stays kv.visit, unchanged, and it is safe to call it while waiting
# even though it INCREMENTS a counter this script later asserts is 1: `probe`
# opens with `raw_call kv.clear` to start from a known state, so nothing the
# wait does survives into the assertion. Both the old loop and this one stop at
# the first success, so the number of successful pre-assertion calls is also
# unchanged.
# shellcheck source=/dev/null
source "$ROOT/tests/lib/e2e_stack.sh"
_dev_ping() {
  curl -sf -o /dev/null -m 5 -X POST -H 'content-type: application/json' \
    "http://localhost:$DEV_PORT/__zeroship/v1/kv.visit" -d '{"json":{}}'
}
stack_wait_dev "dev server" "$WORK/dev.log" _dev_ping || true
RAWFILE="$WORK/dev.raw"; : > "$RAWFILE"
probe "http://localhost:$DEV_PORT" > "$WORK/dev.txt" 2>&1
grep -q '"visits":1' "$WORK/dev.txt" && pass "dev server answered the probe" \
  || { fail "dev server never answered"; tail -20 "$WORK/dev.log"; exit 1; }

# --- 3. deployed side ---
docker rm -f "$REDIS_CONTAINER" >/dev/null 2>&1 || true
docker run --name "$REDIS_CONTAINER" -d -p "$REDIS_PORT:6379" redis:7-alpine >/dev/null
for _ in $(seq 1 30); do docker exec "$REDIS_CONTAINER" redis-cli ping 2>/dev/null | grep -q PONG && break; sleep 1; done
docker exec "$REDIS_CONTAINER" redis-cli ping 2>/dev/null | grep -q PONG \
  && pass "ephemeral Redis ready on :$REDIS_PORT" || { fail "Redis never became ready"; exit 1; }

for p in $ZEROSHIP_CONTROL_PORT $ZEROSHIP_WORKER_PORT $ZEROSHIP_GATEWAY_PORT; do lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "DROP DATABASE IF EXISTS $PG_DB WITH (FORCE)" >/dev/null 2>&1 || true
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "CREATE DATABASE $PG_DB" >/dev/null 2>&1 || true
zs_platform_migrate "$DB_URL" --migrations-dir "$ROOT/db/migrations-ts" \
  --project-schema zeroship --project-id zeroship > "$WORK/migrate.log" 2>&1 \
  || { fail "platform migrations failed"; tail -20 "$WORK/migrate.log"; exit 1; }

ZEROSHIP_GATEWAY_BROKER_SECRET_FILE="$WORK/gate-secret"
e2e_export_runtime_secrets "$WORK" || exit 1
e2e_export_database_urls "$DB_URL"
e2e_start_cdc_relay "$BIN/zeroship-data-cdc-server" || exit 1
"$BIN/zeroship-control" --port "$ZEROSHIP_CONTROL_PORT" --blob-store "$WORK/bundles" \
 > "$WORK/control.log" 2>&1 & PIDS+=($!)
sleep 4
# Without ZEROSHIP_WORKER_KV_CONFIG the env.kv namespace is absent BY DESIGN and every handler
# fails loudly (crates/zeroship-worker/src/main.rs). Omitting it here would look like an
# app bug, not a harness bug.
ZEROSHIP_WORKER_KV_CONFIG="backend = \"redis\"
[redis.topology]
mode = \"standalone\"
endpoint = \"127.0.0.1:$REDIS_PORT\"" \
"$BIN/zeroship-worker" --port "$ZEROSHIP_WORKER_PORT" --threads 2 --control-url "http://localhost:$ZEROSHIP_CONTROL_PORT" \
 --blob-store "$WORK/bundles" --poll-interval 2 \
 > "$WORK/worker.log" 2>&1 & PIDS+=($!)
sleep 3
openssl rand -base64 48 > "$WORK/gate-secret"; chmod 600 "$WORK/gate-secret"
"$BIN/zeroship-gate" --port "$ZEROSHIP_GATEWAY_PORT" --control-url "http://localhost:$ZEROSHIP_CONTROL_PORT" \
 --worker-urls "http://localhost:$ZEROSHIP_WORKER_PORT" --blob-store "$WORK/bundles" \
  --broker-secret-file "$WORK/gate-secret" --poll-interval 2 > "$WORK/gate.log" 2>&1 & PIDS+=($!)
sleep 4
for svc in "control:$ZEROSHIP_CONTROL_PORT" "worker:$ZEROSHIP_WORKER_PORT" "gateway:$ZEROSHIP_GATEWAY_PORT"; do
  curl -sf "http://localhost:${svc##*:}/readyz" >/dev/null \
    || { fail "${svc%%:*} did not come up"; tail -20 "$WORK/${svc%%:*}.log"; exit 1; }
done
pass "control + worker + gateway healthy"

OUT=$("$BIN/dev-provision" --db "$DB_URL" --blob-store "$WORK/bundles" --name "$APP_NAME" --zship "$ZSHIP" 2>&1)
APP_ID=$(echo "$OUT" | awk -F= '$1 == "app_id" { print $2 }')
[ -n "$APP_ID" ] && pass "deployed $APP_NAME" || { fail "provision: $OUT"; exit 1; }
sleep 5   # gateway route-sync poll

RAWFILE="$WORK/deployed.raw"; : > "$RAWFILE"
probe "http://localhost:$ZEROSHIP_GATEWAY_PORT/apps/$APP_NAME" > "$WORK/deployed.txt" 2>&1
grep -q '"visits":1' "$WORK/deployed.txt" && pass "deployed app answered the probe" \
  || { fail "deployed app never answered"; head -5 "$WORK/deployed.txt"; tail -5 "$WORK/worker.log"; }

# ---------------------------------------------------------------------------
# 4. ABSOLUTE verdicts on the DEPLOYED answers.
#
# Read the header note first. Everything below is asserted against the RAW
# deployed body and does not consult dev at all, so it still fires when both
# tiers are wrong in the same way -- the case the section-5 diff cannot report.
# Expectations come from the FIXTURE CONTRACT (examples/kv-dashboard/src/
# index.ts: RATE_LIMIT=5, CACHE_TTL_MS=30s, and the probe's own 60s/120s
# arguments), never from a previous run's output: pinning observed output would
# bless whatever the platform does today, which is the mistake this section
# exists to correct.
# ---------------------------------------------------------------------------
echo ""
echo "--- absolute verdicts on the DEPLOYED answers (dev not consulted) ---"
DR="$WORK/deployed.raw"

drow() { grep -m1 "^$1 " "$DR"; }

# THE CONTROL. Every "field X has value Y" verdict below is safe only if the
# row exists and carries a real result; without it, a run where the gateway
# 404'd every call would report a wall of green `reject` verdicts and a wall of
# red `want` ones that all mean "never measured". A successful RPC body is
# `{"json":...}`; a refusal is a bare `{"message":...,"name":...}`
# (sdks/bootstrap/src/fetch-handler.ts errorBodyFromThrown), so the shape is
# the discriminator.
ROWS_WANT=20
rows_got=$(wc -l < "$DR")
rows_ok=$(grep -c ' {"json":' "$DR")
if [ "$rows_got" -eq "$ROWS_WANT" ] && [ "$rows_ok" -eq "$ROWS_WANT" ]; then
  pass "CONTROL: all $ROWS_WANT deployed rows returned a result envelope"
  ABS_OK=1
else
  fail "CONTROL: $rows_ok of $rows_got deployed rows are results (want $ROWS_WANT) -- verdicts below are UNSAFE"
  grep -v ' {"json":' "$DR" | head -3
  ABS_OK=0
fi

# want <label> <what> <fixed-string that must be IN the row>
want() {
  local row; row="$(drow "$1")"
  if printf '%s' "$row" | grep -qF "$3"; then pass "deployed $1: $2"
  else fail "deployed $1: $2 -- got $(printf '%s' "$row" | cut -c1-260)"; fi
}
# reject <label> <what> <fixed-string that must NOT be in the row>
reject() {
  local row; row="$(drow "$1")"
  if printf '%s' "$row" | grep -qF "$3"; then
    fail "deployed $1: $2 -- got $(printf '%s' "$row" | cut -c1-260)"
  else pass "deployed $1: $2"; fi
}
# band <label> <field> <lo> <hi> -- an integer field in (lo, hi]. This is what
# the `ttlMs`/`resetMs` scrub blanks: a TTL of the right SHAPE and the wrong
# MAGNITUDE reads identically on both tiers.
band() {
  local v; v="$(drow "$1" | grep -oE "\"$2\":-?[0-9]+" | head -1 | cut -d: -f2)"
  if [ -n "$v" ] && [ "$v" -gt "$3" ] && [ "$v" -le "$4" ]; then
    pass "deployed $1: $2=$v is in ($3, $4]"
  else
    fail "deployed $1: $2='${v:-<absent/non-numeric>}' is NOT in ($3, $4] -- $(drow "$1" | cut -c1-200)"
  fi
}
# idem <labelA> <labelB> <field-regex> <what> -- the field must be BYTE-EQUAL
# across two rows. This is the memoisation contract, and the nonce/price/
# builtAt scrubs erase it on both sides at once.
idem() {
  local a b
  a="$(drow "$1" | grep -oE "$3" | head -1)"
  b="$(drow "$2" | grep -oE "$3" | head -1)"
  if [ -n "$a" ] && [ "$a" = "$b" ]; then pass "deployed $4 ($1 == $2: $a)"
  else fail "deployed $4 -- $1 has '${a:-<absent>}', $2 has '${b:-<absent>}'"; fi
}

# incr must actually increment, not saturate.
want visit2 'the second visit counts 2' '"visits":2'

# The limiter: RATE_LIMIT=5, so the 6th hit in the window is refused. A limiter
# that never refuses agrees with itself on both tiers.
want rate1 'first hit counted'            '"count":1'
want rate5 'fifth hit still allowed'      '"allowed":true'
want rate5 'fifth hit exhausts the quota' '"remaining":0'
want rate6 'sixth hit REFUSED'            '"allowed":false'
want rate6 'sixth hit counted'            '"count":6'
band rate6 resetMs 0 60000

# The quote cache: second call is a HIT and returns the FIRST call's value.
want cache1 'first quote is a miss' '"source":"miss"'
want cache2 'second quote is a HIT' '"source":"hit"'
idem cache1 cache2 '"price":[0-9.]+' 'the cache HIT returns the cached price'
band cache2 ttlMs 0 30000

# getOrSet: same contract, and the one the `nonce` scrub hides completely.
want memo1 'first memo is a miss' '"source":"miss"'
want memo2 'second memo is a HIT' '"source":"hit"'
idem memo1 memo2 '"nonce":"[^"]*"'  'the memo HIT returns the memoised nonce'
idem memo1 memo2 '"builtAt":[0-9]+' 'the memo HIT returns the memoised builtAt'

# setIfAbsent under contention: owner-b must NOT take a lease owner-a holds.
want lease1 'owner-a acquires'                 '"acquired":true'
want lease1 'the lease names owner-a'          '"owner":"owner-a"'
want lease2 'owner-b is REFUSED'               '"acquired":false'
want lease2 'the lease still names owner-a'    '"owner":"owner-a"'
reject lease2 'owner-b did not steal the lease' '"owner":"owner-b"'
want leaseC 'clear leaves no lease'            '"lease":null'

# TTL magnitude, set/expire/persist/delete. The probe asks for 60s then 120s.
want strSet 'the value round-trips' '"value":"hello probe"'
want strSet 'the key is present'    '"has":true'
band strSet ttlMs 55000 60000
want strExp 'expire reports it updated' '"updated":true'
band strExp ttlMs 115000 120000
# persist must REMOVE the expiry. `"ttlMs":null` is the only shape the scrub
# does not blank, so a persist that left a number reads as `<V>` on both tiers.
want strPer 'persist clears the expiry' '"ttlMs":null'
want strPer 'persist reports it updated' '"updated":true'
want strDel 'delete reports it deleted' '"deleted":true'
want strDel 'the value is gone'         '"value":null'
want strDel 'the key is absent'         '"has":false'

# The listing must reflect the writes AND the deletes. A list that answered an
# empty page, or kept deleted keys, would diff clean.
want   keys 'lists the visit counter'   'kv-demo:counter:visits'
want   keys 'lists the cached quote'    'kv-demo:cache:quote:probe-sku'
reject keys 'the deleted string is gone' 'kv-demo:strings:greeting'
reject keys 'the cleared lease is gone'  'kv-demo:leases:deploy'

[ "$ABS_OK" = "1" ] || echo "  (verdicts above are UNSAFE: the control failed)"
echo ""

# --- 5. THE RELATIVE COMPARISON: identical operations, identical results? ---
#     NOTE: green here does NOT mean the platform is right. It means the two
#     tiers agree. Section 4 is the half that answers "right".
if diff -q "$WORK/dev.txt" "$WORK/deployed.txt" >/dev/null 2>&1; then
  pass "dev and deployed results are identical across every probed operation"
else
  fail "dev and deployed DIVERGE -- results below (< dev, > deployed)"
  diff "$WORK/dev.txt" "$WORK/deployed.txt" | head -40
  echo ""
  zs_report_staleness_here
  echo "  A divergence here is the finding, not a flaky test. Both backends are"
  echo "  individually correct; disagreeing is the defect. See"
  echo "  docs/pilot/e2e-scenarios.md before weakening anything above."
fi

# --- The floor: a MEASURED minimum, and the guard against a green run over ---
#     nothing. This script exits on $FAIL alone, and $FAIL is 0 both when every
#     assertion passed and when NO assertion ran. The section-4 helpers are the
#     specific hazard: `want`/`reject`/`band`/`idem` all read `drow "$label"`,
#     so a label renamed on one side alone makes the row empty -- and `reject`
#     PASSES on an empty row, because the string it forbids is indeed not there.
#     A capture that went entirely missing would therefore turn some verdicts
#     green rather than red. This repo has shipped three gates that passed over
#     zero tests (#102/#103/#112).
#
# THE FLOOR IS A MEASUREMENT. Taken 2026-08-10 on this tree, running this script
# unmodified against the compose Postgres on :5440 and an ephemeral Redis:
#
#     dev vs deployed: 44 passed, 0 failed        (exit 0)
#
# CROSS-CHECKED against a second, independent instrument: counting CALL SITES in
# the source rather than outcomes in the run. 7 top-level `pass` sites before
# section 4 (.zship built, auth postures, dev probe, Redis ready, three services
# healthy, deployed, deployed probe) + 1 section-4 CONTROL + 35 unconditional
# `want`/`reject`/`band`/`idem` invocations at column 0 + 1 section-5 diff = 44.
# The two agree, and they fail differently: the dynamic count moves when a tier
# stops answering, the static one when an assertion leaves the file.
#
# NO HEADROOM, deliberately -- the total is fixed by the source, not discovered
# at run time, so adding an assertion passes untouched and removing one costs a
# deliberate edit here.
#
# WHAT THE FLOOR DOES NOT CATCH: substitution. Swapping one assertion for an
# easier one keeps the total at 44. Nothing here can see that; review can.
KV_MIN_PASSED=44

echo ""
echo "  dev vs deployed: $PASS passed, $FAIL failed  (mutation: $MUTATE)  (floor $KV_MIN_PASSED)"
echo "  MUTATIONS: MUTATE=rate-never-limits | cache-recompute | ttl-clamped | delete-noop"
echo "  each must leave section 5 GREEN and turn its own section-4 verdicts RED"
echo ""
echo "  --- raw deployed bodies (verbatim) ---"
cut -c1-220 "$WORK/deployed.raw" | sed 's/^/  /'

# A MUTATION run is EXPECTED to fail section-4 verdicts, so the floor is what
# still has to hold there: a mutation should flip verdicts, not remove them.
rc=0
[ "$FAIL" -eq 0 ] || rc=1
# THE FLOOR COUNTS ASSERTIONS THAT RAN, NOT ASSERTIONS THAT PASSED.
#
# It used to test `$PASS`, and with NO HEADROOM (floor 44 == the unmutated
# total, deliberately) that made every mutation control report a second,
# spurious failure. Measured 2026-08-11, MUTATE=rate-never-limits:
#
#     dev vs deployed: 43 passed, 1 failed  (mutation: rate-never-limits)  (floor 44)
#     FAIL: only 43 assertions passed, fewer than the 44 this gate expects.
#
# The mutation did exactly what it promises - `FAIL deployed rate6: sixth hit
# REFUSED` is the control firing - and the floor then blamed the run for lost
# coverage that was never lost. Someone checking whether this harness is
# load-bearing sees two failures and has to work out which one is real.
#
# PASS+FAIL is the invariant the floor actually wants. Failing assertions are
# already caught by `FAIL -eq 0` on the line above; the floor exists for
# assertions that STOPPED RUNNING - a removed check, or a deployed capture that
# lost rows so the reject verdicts pass over empty input. Both still drop
# PASS+FAIL. A mutation does not: it moves an outcome from one column to the
# other. Unmutated 44+0 = 44; rate-never-limits 43+1 = 44.
#
# Same defect, same week, in e2e_dev_vs_deployed_auth.sh (f9b597c10), where it
# was patched with a mode-specific floor because that harness genuinely SKIPS an
# assertion under mutation. This one does not skip; it flips. PASS+FAIL is the
# better fix and needs no knowledge of the mode.
RAN=$((PASS + FAIL))
if [ "$RAN" -lt "$KV_MIN_PASSED" ]; then
  echo "FAIL: only $RAN assertions RAN, fewer than the $KV_MIN_PASSED this gate expects." >&2
  echo "      (passed $PASS, failed $FAIL -- the floor counts both, because a failing" >&2
  echo "      assertion still ran and is already caught above.)" >&2
  echo "      Assertions do not vanish by accident: either the deployed capture lost" >&2
  echo '      rows (in which case reject verdicts are passing on EMPTY rows and mean' >&2
  echo "      nothing) or an assertion was removed. If the removal was deliberate," >&2
  echo "      lower KV_MIN_PASSED in the same change and say why." >&2
  rc=1
fi
exit "$rc"
