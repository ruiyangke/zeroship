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
#   cargo build --release -p zeroship-control -p zeroship-worker -p zeroship-gateway -p zeroship --bins
#   cargo build --release -p zeroship-migrate-adapter --features platform-cli --bin zeroship-platform-migrate
#   docker (Postgres on :5440 as compose-postgres-1, plus an ephemeral Redis)
#   pnpm install in examples/kv-dashboard
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release"
APP="$ROOT/examples/kv-dashboard"
ZSHIP="$APP/dist/app.zship"
WORK="$(mktemp -d)"

PG_CONTAINER="${PG_CONTAINER:-compose-postgres-1}"
PG_USER="${PG_USER:-postgres}"
PG_DB="${PG_DB:-zeroship_devdeploy}"
DB_URL="${DATABASE_URL:-postgres://postgres:zeroship@localhost:5440/$PG_DB}"
# Distinct from golden_path.sh (9390/8390/8300) so both can run at once.
CONTROL_PORT="${CONTROL_PORT:-9392}"
WORKER_PORT="${WORKER_PORT:-8392}"
GATE_PORT="${GATE_PORT:-8302}"
DEV_PORT="${DEV_PORT:-3011}"
REDIS_PORT="${REDIS_PORT:-6396}"
REDIS_CONTAINER="zs-devdeploy-redis"
CONTROL_KEY="dd-ck"; MASTER_KEY="dd-mk"
export ZEROSHIP_DEV_INSECURE=1
export WORKER_KEY="${WORKER_KEY:-devdeploy-worker-key-0123456789abcd}"
APP_NAME="kvdash"
MUTATE="${MUTATE:-none}"

PASS=0; FAIL=0; PIDS=()
pass() { PASS=$((PASS+1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }
cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  lsof -ti :"$DEV_PORT" 2>/dev/null | xargs -r kill 2>/dev/null || true
  docker rm -f "$REDIS_CONTAINER" >/dev/null 2>&1 || true
  # The mutations edit a tracked source file. Restore it whatever happens,
  # including on the exit paths that abort mid-run.
  [ -f "${MUTATE_BAK:-}" ] && cp "$MUTATE_BAK" "$APP/src/index.ts"
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
  "crates/plugin-kv/src crates/runtime/src crates/worker/src crates/gateway/src crates/control/src sdks/kv/src" \
  "zeroship zeroship-worker zeroship-gate zeroship-control dev-provision" \
  || { [ "$?" -eq 2 ] && exit 2; }

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
  local url="$1" hdr="${2:-}" rpc="$1/__zeroship/v1"
  local h=(); [ -n "$hdr" ] && h=(-H "$hdr")
  raw_call() {
    curl -sS -m 20 -X POST -H 'content-type: application/json' "${h[@]}" \
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
lsof -ti :"$DEV_PORT" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
( cd "$APP" && ./node_modules/.bin/vite > "$WORK/dev.log" 2>&1 ) & PIDS+=($!)
for _ in $(seq 1 20); do
  curl -sf -o /dev/null -m 2 -X POST -H 'content-type: application/json' \
    "http://localhost:$DEV_PORT/__zeroship/v1/kv.visit" -d '{"json":{}}' && break
  sleep 2
done
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

for p in $CONTROL_PORT $WORKER_PORT $GATE_PORT; do lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "DROP DATABASE IF EXISTS $PG_DB WITH (FORCE)" >/dev/null 2>&1 || true
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "CREATE DATABASE $PG_DB" >/dev/null 2>&1 || true
"$BIN/zeroship-platform-migrate" --database-url "$DB_URL" --migrations-dir "$ROOT/db/migrations-ts" \
  --project-schema zeroship --project-id zeroship > "$WORK/migrate.log" 2>&1 \
  || { fail "platform migrations failed"; tail -20 "$WORK/migrate.log"; exit 1; }

"$BIN/zeroship-control" --port "$CONTROL_PORT" --db "$DB_URL" --blob-store "$WORK/bundles" \
  --control-key "$CONTROL_KEY" --master-key "$MASTER_KEY" > "$WORK/control.log" 2>&1 & PIDS+=($!)
sleep 4
# Without --kv-url the env.kv namespace is absent BY DESIGN and every handler
# fails loudly (crates/worker/src/main.rs). Omitting it here would look like an
# app bug, not a harness bug.
"$BIN/zeroship-worker" --port "$WORKER_PORT" --worker-threads 2 --control "http://localhost:$CONTROL_PORT" \
  --control-key "$CONTROL_KEY" --blob-store "$WORK/bundles" --poll-interval 2 \
  --kv-url "redis://127.0.0.1:$REDIS_PORT" > "$WORK/worker.log" 2>&1 & PIDS+=($!)
sleep 3
openssl rand -base64 48 > "$WORK/gate-secret"; chmod 600 "$WORK/gate-secret"
"$BIN/zeroship-gate" --port "$GATE_PORT" --control "http://localhost:$CONTROL_PORT" \
  --control-key "$CONTROL_KEY" --workers "http://localhost:$WORKER_PORT" --blob-store "$WORK/bundles" \
  --gateway-broker-secret-file "$WORK/gate-secret" --poll-interval 2 > "$WORK/gate.log" 2>&1 & PIDS+=($!)
sleep 4
for svc in "control:$CONTROL_PORT" "worker:$WORKER_PORT" "gateway:$GATE_PORT"; do
  curl -sf "http://localhost:${svc##*:}/health" >/dev/null \
    || { fail "${svc%%:*} did not come up"; tail -20 "$WORK/${svc%%:*}.log"; exit 1; }
done
pass "control + worker + gateway healthy"

OUT=$("$BIN/dev-provision" --db "$DB_URL" --blob-store "$WORK/bundles" --name "$APP_NAME" --zship "$ZSHIP" 2>&1)
API_KEY=$(echo "$OUT" | awk -F= '$1 == "api_key" { print $2 }')
[ -n "$API_KEY" ] && pass "deployed $APP_NAME" || { fail "provision: $OUT"; exit 1; }
sleep 5   # gateway route-sync poll

RAWFILE="$WORK/deployed.raw"; : > "$RAWFILE"
probe "http://localhost:$GATE_PORT/apps/$APP_NAME" "X-Api-Key: $API_KEY" > "$WORK/deployed.txt" 2>&1
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

echo ""
echo "  dev vs deployed: $PASS passed, $FAIL failed  (mutation: $MUTATE)"
echo "  MUTATIONS: MUTATE=rate-never-limits | cache-recompute | ttl-clamped | delete-noop"
echo "  each must leave section 5 GREEN and turn its own section-4 verdicts RED"
echo ""
echo "  --- raw deployed bodies (verbatim) ---"
cut -c1-220 "$WORK/deployed.raw" | sed 's/^/  /'
[ "$FAIL" -eq 0 ]
