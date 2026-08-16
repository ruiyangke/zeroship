#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# env.storage, dev vs deployed: run ONE identical operation sequence against
# `pnpm dev` and against the same app deployed behind the gateway, then diff
# the RESULTS.
#
# Scenario 5 of docs/pilot/e2e-scenarios.md was "DEV only" for a reason that
# was not about storage: the only storage fixture (examples/auth-uploads-kv)
# authenticates through the dev-auth provider, which is dev-only by
# construction, so its deployed side could not be driven at all. Storage has no
# auth dependency, so examples/storage-probe removes the coupling -- no login,
# every procedure anonymous -- and this script walks both sides.
#
# The seam this is pointed at: `pnpm dev` always runs env.storage on LocalFs
# (crates/cli/src/main.rs, `file://.zeroship/storage` unless
# ZEROSHIP_WORKER_STORAGE_URL says otherwise), while a deployed worker runs whatever
# --storage-url names, which in production is S3/R2. So the default here
# deploys against a MinIO container, making the comparison LocalFs-vs-S3 rather
# than LocalFs-vs-LocalFs. That is where this project's one historical storage
# seam bug lived (LocalFs dropped contentType while S3 kept it), and a
# same-backend comparison could not have seen it.
#
#   DEPLOYED_STORAGE=s3    (default) worker --storage-url s3://<minio>
#   DEPLOYED_STORAGE=file            worker --storage-url <local dir>
#
# Running both answers a question one run cannot: whether a divergence comes
# from the BACKEND or from the dev-vs-deployed request path.
#
# Proving it is load-bearing -- two mutations, each of a real thing:
#   MUTATE=no-storage-url   boot the deployed worker with no --storage-url, so
#                           the env.storage namespace is absent by design and
#                           every deployed procedure fails. (The kv harness
#                           learned this with ZEROSHIP_WORKER_KV_URL.)
#   MUTATE=no-config        build without src/server/config.ts, so every
#                           procedure resolves to `auth: "user"` and the
#                           gateway refuses all fourteen -- green in dev, 401
#                           deployed. That is how kv-dashboard shipped (#163).
# Both must turn the diff RED. If they do not, this script is decorative.
#
# WHY SECTION 3b EXISTS (added 2026-08-09). The diff answers "do LocalFs and S3
# AGREE"; it cannot answer "is the answer RIGHT". Both backends dropping
# contentType, both listing an unfiltered prefix, both storing 512 KiB of a
# 1 MiB upload -- every one of those produces byte-identical rows and a green
# run. That failure mode is not hypothetical: production returned `Error.stack`
# to anonymous callers while `e2e_dev_vs_deployed_auth.sh` stayed green because
# dev leaked the same stack (39f6aa350; docs/pilot/e2e-scenarios.md, "What a
# dev-vs-deployed comparison cannot see"). This harness normalises NOTHING, so
# there is no scrub to blame -- two tiers agreeing on a wrong value produce an
# empty diff with no normalisation at all.
#
# So section 3b asserts the DEPLOYED row against the fixture's own contract,
# with dev not consulted. Three further mutations prove those verdicts are not
# green-by-construction. Each edits the app source BOTH tiers build from, so
# the diff stays GREEN and only the named verdict moves -- which is the blind
# spot itself, demonstrated:
#   MUTATE=text-no-ctype    the text put stops declaring a content type
#   MUTATE=list-prefix-broad  the prefix listing stops filtering
#   MUTATE=stream-corrupt   the streamed bytes stop matching their checksum
#
# Prereqs (docs/runbooks/local-dev.md):
#   cargo build --release -p zeroship-control -p zeroship-worker -p zeroship-gateway -p zeroship --bins
#   cargo build --release -p zeroship-migrate-adapter --features platform-cli --bin zeroship-platform-migrate
#   docker (Postgres on :5440 as compose-postgres-1; a MinIO container for the
#     default s3 mode, image minio/minio)
#   pnpm install in examples/storage-probe
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release"
# shellcheck source=tests/lib/runtime_secrets.sh
source "$ROOT/tests/lib/runtime_secrets.sh"
APP="$ROOT/examples/storage-probe"
ZSHIP="$APP/dist/app.zship"
WORK="$(mktemp -d)"

PG_CONTAINER="${PG_CONTAINER:-compose-postgres-1}"
PG_USER="${PG_USER:-postgres}"
PG_DB="${PG_DB:-zeroship_storageleg}"
DB_URL="${DATABASE_URL:-postgres://postgres:zeroship@localhost:5440/$PG_DB}"
# A band of its own: kv uses 9392/8392/8302/3011, streaming 9394/8394/8304/3061,
# golden_path 9390/8390/8300 -- so all four can run at once.
ZEROSHIP_CONTROL_PORT="${ZEROSHIP_CONTROL_PORT:-9396}"
ZEROSHIP_WORKER_PORT="${ZEROSHIP_WORKER_PORT:-8396}"
ZEROSHIP_GATEWAY_PORT="${ZEROSHIP_GATEWAY_PORT:-8306}"
DEV_PORT="${DEV_PORT:-3081}"
# VITE's own port. DEV_PORT above is the RUNTIME port -- what `zeroship serve`
# binds. vite was silently taking its :5173 global default, which nothing here
# declared, tracked or freed, so a second harness on this machine fought it for
# the port and the cleanup trap could never reclaim it (#173's class). Private,
# and --strictPort at the call so a conflict fails loudly rather than moving to
# a port nobody watches. See #272.
VITE_PORT="${VITE_PORT:-5081}"
MINIO_PORT="${MINIO_PORT:-9203}"
MINIO_CONTAINER="zs-devdeploy-storage-minio"
MINIO_ACCESS="minioadmin"; MINIO_SECRET="minioadmin"; MINIO_BUCKET="zeroship-storage-probe"
ZEROSHIP_CONTROL_KEY="sp-ck"; ZEROSHIP_CONTROL_MASTER_KEY="sp-mk"
export ZEROSHIP_WORKER_KEY="${ZEROSHIP_WORKER_KEY:-storageprobe-worker-key-0123456789ab}"
APP_NAME="storagep"
DEPLOYED_STORAGE="${DEPLOYED_STORAGE:-s3}"
MUTATE="${MUTATE:-none}"
# The worker's --storage-url also reads this variable, so an inherited value
# from the caller's shell would quietly become the deployed backend. Each side
# gets its backend explicitly below; nothing here may come from the ambient
# environment.
unset ZEROSHIP_WORKER_STORAGE_URL

PASS=0; FAIL=0; PIDS=()
pass() { PASS=$((PASS+1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }
cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  # BOTH ports, by LISTENER not by recorded PID: `( cd x && vite )&` records the
  # subshell, and vite outlives it as an orphan the PID loop above cannot reach.
  for _p in "$DEV_PORT" "$VITE_PORT"; do
    lsof -ti :"$_p" 2>/dev/null | xargs -r kill 2>/dev/null || true
  done
  docker rm -f "$MINIO_CONTAINER" >/dev/null 2>&1 || true
  # Restore the config file if the no-config mutation moved it.
  [ -f "$WORK/config.ts.bak" ] && cp "$WORK/config.ts.bak" "$APP/src/server/config.ts"
  # ...and the app source if a source mutation edited it.
  [ -f "${MUTATE_BAK:-}" ] && cp "$MUTATE_BAK" "$APP/src/index.ts"
  # KEEP_WORK=1 leaves the service logs behind. A storage divergence is almost
  # always explained by a line in worker.log, and that line is gone by the time
  # the diff is on screen otherwise.
  [ "${KEEP_WORK:-0}" = "1" ] && { echo "  work dir kept: $WORK"; return; }
  rm -rf "$WORK"
}
trap cleanup EXIT

echo "=== env.storage: dev vs deployed (storage-probe) ==="
echo "  deployed backend: $DEPLOYED_STORAGE   mutation: $MUTATE"

# --- 0. the two sides must be the same BUILD -------------------------------
#
# This check exists because its absence already produced a false finding. The
# dev side runs `target/release/zeroship` and the deployed side runs
# `target/release/zeroship-worker`; they are separate binaries and a partial
# rebuild leaves them at different commits. On the first run of this script the
# worker predated 3e7e5e387 ("page env.storage.list"), so it still returned a
# bare ARRAY where the SDK now expects `{entries, cursor}` -- and every list
# operation "diverged" with `r.entries.map is not a function`. That reads
# exactly like an S3 backend defect and is not one.
#
# Shared with the other dev-vs-deployed harnesses, which have the same blind
# spot. `dev` runs vite, which spawns `zeroship serve` (sdks/vite-plugin/src/
# dev-server.ts), so the CLI binary is genuinely part of the dev side and
# belongs in the list beside the server binaries.
# shellcheck source=lib/binary_freshness.sh
source "$ROOT/tests/lib/binary_freshness.sh"
zs_check_binary_freshness "$ROOT" "$BIN" \
  "crates/plugin-storage/src crates/runtime/src crates/worker/src crates/gateway/src crates/control/src libs/compio-s3/src sdks/storage/src" \
  "zeroship zeroship-worker zeroship-gate zeroship-control dev-provision" \
  || { _zs_fresh_rc=$?; [ "$_zs_fresh_rc" -ne 0 ] && exit "$_zs_fresh_rc"; }

# --- the probe: one deterministic sequence, printed as RESULTS -------------
#
# Nothing here is normalised away. The app was written so that nothing volatile
# reaches the wire: no timestamps (modifiedAt is dropped in the handler), no
# cursors (documented opaque, so pagination is driven INSIDE the procedure and
# only the page boundaries come back), no backend identifiers. Every field
# printed below is therefore being asserted identical across LocalFs and S3.
probe() {
  local base="$1" hdr="${2:-}" rpc="$1/__zeroship/v1"
  local h=(); [ -n "$hdr" ] && h=(-H "$hdr")
  call() {
    curl -sS -m 60 -X POST -H 'content-type: application/json' "${h[@]}" \
      "$rpc/$1" -d "{\"json\":${2:-{\}}}" 2>&1
  }
  echo "reset        $(call probe.reset)"
  echo "text         $(call probe.text)"
  echo "binary       $(call probe.binary)"
  echo "overwrite    $(call probe.overwrite)"
  echo "deleteAbsent $(call probe.deleteAbsent)"
  echo "contentTypes $(call probe.contentTypes)"
  echo "listPrefix   $(call probe.listPrefix)"
  echo "listPaginate $(call probe.listPaginate)"
  echo "listOvershoot $(call probe.listOvershoot)"
  echo "streamPut    $(call probe.streamPut)"
  echo "streamGet    $(call probe.streamGet)"
  echo "streamAbsent $(call probe.streamGetAbsent)"
  echo "streamBuf    $(call probe.streamThenBuffered)"
}

# --- 1. real build ---------------------------------------------------------
if [ "$MUTATE" = "no-config" ]; then
  cp "$APP/src/server/config.ts" "$WORK/config.ts.bak"
  rm "$APP/src/server/config.ts"
  echo "  (MUTATION: src/server/config.ts removed before build)"
fi
# Source mutations for the ABSOLUTE verdicts (section 3b). Both tiers build
# from the edited file on purpose: a one-sided edit would show up in the diff
# and would prove nothing about the case the diff cannot see.
case "$MUTATE" in
  text-no-ctype|list-prefix-broad|stream-corrupt)
    MUTATE_BAK="$WORK/index.ts.bak"
    cp "$APP/src/index.ts" "$MUTATE_BAK"
    case "$MUTATE" in
      text-no-ctype)
        sed -i 's|store().put(key, TEXT, { contentType: "text/plain; charset=utf-8" })|store().put(key, TEXT)|' \
          "$APP/src/index.ts"
        grep -q 'store().put(key, TEXT))' "$APP/src/index.ts" \
          || { fail "mutation did not apply"; exit 1; } ;;
      list-prefix-broad)
        # The prefix stops filtering; every key under the base comes back.
        sed -i 's|store().list(`${base}a/`, { limit: 50 })|store().list(base, { limit: 50 })|' \
          "$APP/src/index.ts"
        grep -q 'const underA = must(await store().list(base, { limit: 50 }));' "$APP/src/index.ts" \
          || { fail "mutation did not apply"; exit 1; } ;;
      stream-corrupt)
        # The bytes SENT diverge from the bytes checksummed: one flipped byte
        # per chunk, after the running checksum has seen the original. Sizes
        # and chunk counts are untouched, so only the checksum verdicts move.
        sed -i 's|updateChecksum(acc, chunk, offset);|updateChecksum(acc, chunk, offset); chunk[0] ^= 0xff;|' \
          "$APP/src/index.ts"
        grep -q 'chunk\[0\] \^= 0xff;' "$APP/src/index.ts" \
          || { fail "mutation did not apply"; exit 1; } ;;
    esac
    echo "  (MUTATION $MUTATE: both tiers build from the edited source)" ;;
esac
( cd "$APP" && pnpm build ) > "$WORK/build.log" 2>&1
[ -f "$ZSHIP" ] && pass "built app.zship ($(du -k "$ZSHIP" | cut -f1)KB)" \
  || { fail "build produced no app.zship"; tail -20 "$WORK/build.log"; exit 1; }
# A missing "use server" builds clean, reports 0 server functions, and still
# emits a manifest declaring every RPC resource -- so every procedure 404s at
# runtime with no build error (#167). Check the count, not the exit code.
grep -q "14 server functions" "$WORK/build.log" && pass "server bundle carries all 14 procedures" \
  || { fail "server bundle did not report 14 server functions (missing \"use server\"?)"; \
       grep -i "server functions" "$WORK/build.log" || true; }

# Every RPC resource must declare an auth posture, or it resolves to
# `auth: user` and the gateway refuses it -- green in dev, 401 deployed.
d="$WORK/unpack"; mkdir -p "$d"; tar -xf "$ZSHIP" -C "$d"
total=$(grep -oE '"rpc:[^"]+":' "$d/manifest.json" | wc -l)
authed=$(grep -oE '"rpc:[^"]+":\{[^}]*"auth":' "$d/manifest.json" | wc -l)
[ "$total" -gt 0 ] && [ "$authed" -eq "$total" ] \
  && pass "all $total rpc resources declare an auth posture" \
  || fail "only $authed of $total rpc resources declare auth (missing src/server/config.ts?)"

# --- 2. dev side -----------------------------------------------------------
# A fresh LocalFs root per run, so dev starts from the same empty state the
# deployed backend does. Without this the dev tier carries objects over from
# the previous run and `reset` would be doing the comparison's work.
#
# Set on the vite process ONLY, never exported. The worker's --storage-url
# clap arg also reads ZEROSHIP_WORKER_STORAGE_URL, so an exported value silently
# becomes the deployed backend's fallback: with it exported, MUTATE=no-storage-url
# booted a worker that quietly picked up the DEV directory and the mutation
# passed 11/0 -- a mutation that cannot fail proves nothing about the check it
# is meant to be testing.
mkdir -p "$WORK/dev-storage"
for _p in "$DEV_PORT" "$VITE_PORT"; do
  lsof -ti :"$_p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
done
( cd "$APP" && ZEROSHIP_WORKER_STORAGE_URL="file://$WORK/dev-storage" \
    ./node_modules/.bin/vite --port "$VITE_PORT" --strictPort > "$WORK/dev.log" 2>&1 ) & PIDS+=($!)
# Readiness: a DEADLINE plus a diagnosis, not a fixed iteration count (#273).
# The old form was `for _ in $(seq 1 20); do ... sleep 2; done` -- 40 s sized on
# an idle machine -- and it reported "dev app never came up" whatever the cause.
# Measured here on 2026-08-11: run 1 RED, run 2 green, same code, because vite
# was re-optimising dependencies after a lockfile change while the budget ran
# out. The app was fine; the message was not.
#
# SOURCED HERE, not at the top of the file, and that placement is load-bearing.
# tests/lib/e2e_stack.sh opens with `: "${ZEROSHIP_CONTROL_PORT:=9120}"` and four more of
# the same shape. Those only assign when unset -- so sourcing it ABOVE this
# harness's own `ZEROSHIP_CONTROL_PORT="${ZEROSHIP_CONTROL_PORT:-9396}"` block would let the
# library's ports win, silently moving this harness onto another suite's band.
# By here every port this script owns is already set, so the `:=` defaults are
# all no-ops.
# shellcheck source=/dev/null
source "$ROOT/tests/lib/e2e_stack.sh"
_dev_ping() {
  curl -sf -o /dev/null -m 5 -X POST -H 'content-type: application/json' \
    "http://localhost:$DEV_PORT/__zeroship/v1/probe.ping" -d '{"json":{}}'
}
if stack_wait_dev "dev app" "$WORK/dev.log" _dev_ping; then
  pass "dev app reachable"
else
  fail "dev app never became ready -- see the diagnosis and log tail above"
  exit 1
fi
probe "http://localhost:$DEV_PORT" > "$WORK/dev.txt" 2>&1
grep -q '"textMatches":true' "$WORK/dev.txt" && pass "dev side answered the probe" \
  || { fail "dev side did not round-trip text"; head -20 "$WORK/dev.txt"; }

# --- 3. deployed side ------------------------------------------------------
if [ "$DEPLOYED_STORAGE" = "s3" ]; then
  docker rm -f "$MINIO_CONTAINER" >/dev/null 2>&1 || true
  docker run -d --name "$MINIO_CONTAINER" -p "$MINIO_PORT:9000" \
    -e "MINIO_ROOT_USER=$MINIO_ACCESS" -e "MINIO_ROOT_PASSWORD=$MINIO_SECRET" \
    minio/minio server /data >/dev/null 2>&1 || { fail "MinIO container failed to start"; exit 1; }
  ready=0
  for _ in $(seq 1 40); do
    if docker exec "$MINIO_CONTAINER" mc alias set local "http://127.0.0.1:9000" \
         "$MINIO_ACCESS" "$MINIO_SECRET" >/dev/null 2>&1 \
       && docker exec "$MINIO_CONTAINER" mc mb -p "local/$MINIO_BUCKET" >/dev/null 2>&1; then
      ready=1; break
    fi
    sleep 0.5
  done
  [ "$ready" = 1 ] && pass "MinIO ready on :$MINIO_PORT (bucket $MINIO_BUCKET)" \
    || { fail "MinIO never became ready"; docker logs "$MINIO_CONTAINER" 2>&1 | tail -20; exit 1; }
  export AWS_ACCESS_KEY_ID="$MINIO_ACCESS" AWS_SECRET_ACCESS_KEY="$MINIO_SECRET"
  unset AWS_SESSION_TOKEN 2>/dev/null || true
  STORAGE_ARG="s3://$MINIO_BUCKET/storage?provider=minio&endpoint=http://127.0.0.1:$MINIO_PORT&region=us-east-1&style=path&dev_http=true&checksum=none"
else
  mkdir -p "$WORK/deployed-storage"
  STORAGE_ARG="file://$WORK/deployed-storage"
fi

for p in $ZEROSHIP_CONTROL_PORT $ZEROSHIP_WORKER_PORT $ZEROSHIP_GATEWAY_PORT; do lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "DROP DATABASE IF EXISTS $PG_DB WITH (FORCE)" >/dev/null 2>&1 || true
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "CREATE DATABASE $PG_DB" >/dev/null 2>&1 || true
"$BIN/zeroship-platform-migrate" --database-url "$DB_URL" --migrations-dir "$ROOT/db/migrations-ts" \
  --project-schema zeroship --project-id zeroship > "$WORK/migrate.log" 2>&1 \
  || { fail "platform migrations failed"; tail -20 "$WORK/migrate.log"; exit 1; }

ZEROSHIP_GATEWAY_BROKER_SECRET_FILE="$WORK/gate-secret"
e2e_export_runtime_secrets "$WORK" || exit 1
e2e_export_database_urls "$DB_URL"
e2e_with_platform_mint_key "$BIN/zeroship-control" --port "$ZEROSHIP_CONTROL_PORT" --blob-store "$WORK/bundles" \
 > "$WORK/control.log" 2>&1 & PIDS+=($!)
sleep 4
# Without --storage-url the env.storage namespace is absent BY DESIGN
# (crates/worker/src/main.rs:386) and every handler fails loudly. Omitting it
# would look like an app bug rather than a harness bug -- which is exactly the
# MUTATE=no-storage-url case below.
STORAGE_FLAG=(--storage-url "$STORAGE_ARG")
[ "$MUTATE" = "no-storage-url" ] && { STORAGE_FLAG=(); echo "  (MUTATION: deployed worker booted with NO --storage-url)"; }
"$BIN/zeroship-worker" --port "$ZEROSHIP_WORKER_PORT" --threads 2 --control-url "http://localhost:$ZEROSHIP_CONTROL_PORT" \
 --blob-store "$WORK/bundles" --poll-interval 2 \
  "${STORAGE_FLAG[@]}" > "$WORK/worker.log" 2>&1 & PIDS+=($!)
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
API_KEY=$(echo "$OUT" | awk -F= '$1 == "api_key" { print $2 }')
[ -n "$API_KEY" ] && pass "deployed $APP_NAME" || { fail "provision: $OUT"; exit 1; }
sleep 6   # gateway route-sync poll

curl -sf -o /dev/null -m 10 -X POST -H 'content-type: application/json' -H "X-Api-Key: $API_KEY" \
  "http://localhost:$ZEROSHIP_GATEWAY_PORT/apps/$APP_NAME/__zeroship/v1/probe.ping" -d '{"json":{}}' \
  && pass "deployed app reachable" || fail "deployed app did not answer ping"
probe "http://localhost:$ZEROSHIP_GATEWAY_PORT/apps/$APP_NAME" "X-Api-Key: $API_KEY" > "$WORK/deployed.txt" 2>&1
grep -q '"textMatches":true' "$WORK/deployed.txt" && pass "deployed side answered the probe" \
  || { fail "deployed side did not round-trip text"; head -20 "$WORK/deployed.txt"; tail -10 "$WORK/worker.log"; }

# ---------------------------------------------------------------------------
# 3b. ABSOLUTE verdicts on the DEPLOYED answers. Dev is not consulted.
#
# Read the header note first. Every expectation here is derived from the
# FIXTURE CONTRACT (examples/storage-probe/src/index.ts: a 256-byte all-values
# body, "second" overwriting a longer string, five `page/` keys read two at a
# time, a 1 MiB stream in 64 KiB chunks) -- never from a previous run's output.
# Pinning observed output would bless whatever the backends do today, which is
# the mistake this section exists to correct.
#
# What is deliberately NOT asserted: the `given: null` content-type row. The
# SDK types contentType `string | null` and a backend that substitutes a
# default is different rather than wrong, so the two-sided diff is the right
# instrument for it and an absolute pin would invent a contract.
# ---------------------------------------------------------------------------
echo ""
echo "--- absolute verdicts on the DEPLOYED answers (dev not consulted) ---"
DP="$WORK/deployed.txt"
drow() { grep -m1 "^$1 " "$DP"; }

# THE CONTROL. A "field X is Y" verdict only means something if the row is a
# result at all. A successful RPC body is `{"json":...}`; a refusal is a bare
# `{"message":...}` (sdks/bootstrap/src/fetch-handler.ts errorBodyFromThrown).
ROWS_WANT=13
rows_got=$(grep -c . "$DP")
rows_ok=$(grep -c ' {"json":' "$DP")
if [ "$rows_got" -eq "$ROWS_WANT" ] && [ "$rows_ok" -eq "$ROWS_WANT" ]; then
  pass "CONTROL: all $ROWS_WANT deployed rows returned a result envelope"
  ABS_OK=1
else
  fail "CONTROL: $rows_ok of $rows_got deployed rows are results (want $ROWS_WANT) -- verdicts below are UNSAFE"
  grep -v ' {"json":' "$DP" | head -3
  ABS_OK=0
fi

# want <label> <what> <fixed string that must be IN the row>
want() {
  local row; row="$(drow "$1")"
  if printf '%s' "$row" | grep -qF "$3"; then pass "deployed $1: $2"
  else fail "deployed $1: $2 -- got $(printf '%s' "$row" | cut -c1-300)"; fi
}
# idem <labelA> <labelB> <field-regex> <what> -- byte-equal across two rows.
idem() {
  local a b
  a="$(drow "$1" | grep -oE "$3" | head -1)"
  b="$(drow "$2" | grep -oE "$3" | head -1)"
  if [ -n "$a" ] && [ "$a" = "$b" ]; then pass "deployed $4 ($1 == $2: $a)"
  else fail "deployed $4 -- $1 has '${a:-<absent>}', $2 has '${b:-<absent>}'"; fi
}
# eqfields <label> <fieldA> <fieldB> <what> -- two fields of ONE row must carry
# the same number. Order-independent, unlike matching the serialised pair.
eqfields() {
  local row a b
  row="$(drow "$1")"
  a="$(printf '%s' "$row" | grep -oE "\"$2\":[0-9]+" | head -1 | cut -d: -f2)"
  b="$(printf '%s' "$row" | grep -oE "\"$3\":[0-9]+" | head -1 | cut -d: -f2)"
  if [ -n "$a" ] && [ "$a" = "$b" ]; then pass "deployed $1: $4 ($2=$3=$a)"
  else fail "deployed $1: $4 -- $2='${a:-<absent>}' $3='${b:-<absent>}'"; fi
}

want reset 'the root is empty after reset' '"remaining":0'
want reset 'no page remains after reset'   '"moreAfter":false'

# A put that lost its content type is the ONE historical seam bug here, and a
# tier-vs-tier diff went green on it for as long as only one side had it.
want text 'the object is found'        '"found":true'
want text 'the content type survives'  '"contentType":"text/plain; charset=utf-8"'
want text 'the text round-trips'       '"textMatches":true'
eqfields text putSize getSize 'put and get agree on size'

# 256 distinct byte values: a backend that round-trips through a string
# corrupts the high half, and both backends could corrupt it the same way.
want binary 'all 256 bytes are stored'     '"putSize":256'
want binary 'all 256 bytes come back'      '"getSize":256'
want binary 'the bytes are byte-identical' '"bytesIdentical":true'
want binary 'octet-stream survives'        '"contentType":"application/octet-stream"'

want overwrite 'the shorter second write replaces the first' '"secondSize":6'
want overwrite 'the second body is the one that is stored'   '"secondText":"second"'
want overwrite 'the new content type replaces the old'       '"secondType":"application/json"'
want overwrite 'an overwrite does not create a second entry' '"entriesUnderPrefix":1'

want deleteAbsent 'delete of a present key reports deleted' '"firstDeleted":true'
want deleteAbsent 'the key is gone after delete'            '"getAfterDeleteFound":false'
want deleteAbsent 'nothing is left under the prefix'        '"entriesUnderPrefix":0'

# Only the EXPLICIT content types are pinned; see the note above.
want contentTypes 'text/plain round-trips' '{"given":"text/plain; charset=utf-8","got":"text/plain; charset=utf-8"'
want contentTypes 'application/json round-trips' '{"given":"application/json","got":"application/json"'
want contentTypes 'image/png round-trips'  '{"given":"image/png","got":"image/png"'

# The whole array, so membership, ORDER and the excluded sibling (`ab.txt`,
# which shares the `sp/list/a` string prefix but not the `sp/list/a/` one) are
# one verdict. A prefix that stopped filtering would return all five.
want listPrefix 'the prefix filters, and the order is ascending' \
  '"aKeys":["sp/list/a/1.txt","sp/list/a/2.txt","sp/list/a/3.txt"]'
want listPrefix 'entry sizes come back with the keys' '"aSizes":[1,2,3]'
want listPrefix 'a complete page reports a null cursor' '"aCursorNull":true'
want listPrefix 'the base listing is complete and ascending' \
  '"baseKeys":["sp/list/a/1.txt","sp/list/a/2.txt","sp/list/a/3.txt","sp/list/ab.txt","sp/list/b/1.txt"]'

# limit=2 over five keys: three pages, and the cursor is non-null IFF more
# remains. A backend that ignored `limit` would return one page of five and
# still diff clean against another that did the same.
want listPaginate 'pages break at the limit' \
  '"pages":[["sp/page/k0.txt","sp/page/k1.txt"],["sp/page/k2.txt","sp/page/k3.txt"],["sp/page/k4.txt"]]'
want listPaginate 'the cursor is non-null IFF more remains' '"cursorNonNull":[true,true,false]'
want listPaginate 'nothing is dropped or repeated' '"total":5'
want listPaginate 'every key is distinct'          '"unique":5'
want listPaginate 'listAll walks the same keys'    '"listAllMatches":true'
want listOvershoot 'an oversized limit returns one page' '"count":5'
want listOvershoot 'and a null cursor'                   '"cursorNull":true'

# 1 MiB in 64 KiB chunks. `size == expectedSize` is the truncation verdict;
# the checksum equalities are the corruption verdict; `multiChunk` is what
# distinguishes a chunked read from a whole-object one.
eqfields streamPut size expectedSize 'the stream stored every byte it sent'
want streamPut 'the upload was chunked (1 MiB / 64 KiB)' '"chunksSent":16'
want streamGet 'the streamed object reads back'       '"found":true'
want streamGet 'the read was chunked'                 '"multiChunk":true'
idem streamPut streamGet '"checksum":"[0-9a-f]+"' 'the streamed bytes survive the round trip'
eqfields streamGet size declaredSize 'the bytes read match the declared size'
want streamAbsent 'a streaming read of an absent key reports absence' '"found":false'
want streamBuf 'the streamed object is visible to the buffered read' '"found":true'
want streamBuf 'and reports the full size'   '"size":1048576'
idem streamPut streamBuf '"checksum":"[0-9a-f]+"' 'the buffered read sees the same bytes'
want streamBuf 'and lists under its prefix'  '"listedKeys":["sp/stream/blob.bin"],"listedSizes":[1048576]'

[ "$ABS_OK" = "1" ] || echo "  (verdicts above are UNSAFE: the control failed)"
echo ""

# --- 4. THE POINT: identical operations must produce identical results -----
if diff -q "$WORK/dev.txt" "$WORK/deployed.txt" >/dev/null 2>&1; then
  pass "dev and deployed results are identical across every probed operation"
else
  fail "dev and deployed DIVERGE -- results below (< dev, > deployed)"
  diff "$WORK/dev.txt" "$WORK/deployed.txt" | head -60
  echo ""
  zs_report_staleness_here
  echo "  A divergence here is the finding, not a flaky test. Both backends are"
  echo "  individually correct; disagreeing is the defect. See"
  echo "  docs/pilot/e2e-scenarios.md before weakening anything above."
fi

# Kept per RUN, not per harness. A fixed name like /tmp/storage-probe-dev.txt
# is unique against other scripts but NOT against the previous invocation of
# this one, so a baseline run followed by a MUTATE= run leaves the mutation's
# output sitting under the name a reader takes for the baseline's. That is not
# hypothetical: it misled the review of this very script, where a post-mutation
# `diff` of those two files reported DIFFER and briefly looked like the baseline
# comparison had failed.
KEEP="$(mktemp -d /tmp/storage-probe-XXXXXX)"
cp "$WORK/dev.txt" "$KEEP/dev.txt" 2>/dev/null || true
cp "$WORK/deployed.txt" "$KEEP/deployed.txt" 2>/dev/null || true
# --- The floor: a MEASURED minimum, and the guard against a green run over ---
#     nothing. This script exits on $FAIL alone, and $FAIL is 0 both when every
#     assertion passed and when NO assertion ran. This repo has shipped three
#     gates that passed over zero tests (#102/#103/#112).
#
# THE FLOOR IS A MEASUREMENT. Taken 2026-08-10 on this tree, running this script
# unmodified (DEPLOYED_STORAGE=s3, the default) against the compose Postgres on
# :5440 and an ephemeral MinIO:
#
#     env.storage dev vs deployed: 54 passed, 0 failed        (exit 0)
#
# CROSS-CHECKED against a second, independent instrument: CALL SITES in the
# source rather than outcomes in the run. 10 top-level `pass` sites (.zship
# built, 14 server functions, auth postures, dev reachable, dev probe, MinIO
# ready, three services healthy, deployed, deployed reachable, deployed probe)
# + 1 section-3b CONTROL + 42 unconditional `want`/`idem`/`eqfields`
# invocations at column 0 + 1 section-4 diff = 54. Dynamic and static agree,
# and they fail differently: the dynamic count moves when a tier stops
# answering, the static one when an assertion leaves the file.
#
# 53 IS THE LOWER OF THE TWO LEGITIMATE CONFIGURATIONS, which is where a floor
# has to sit -- the same reasoning golden_path.sh applies to its ZEROSHIP_TOKEN
# arm. `DEPLOYED_STORAGE=file` skips the "MinIO ready" pass and yields 53; the
# default s3 mode yields 54. CI runs the s3 mode, so CI sits one above the
# floor, and that one is the only headroom here.
#
# WHAT THE FLOOR DOES NOT CATCH: substitution. Swapping one assertion for an
# easier one keeps the total unchanged. Nothing here can see that; review can.
STORAGE_MIN_PASSED="${STORAGE_MIN_PASSED:-53}"

echo ""
echo "  results kept at $KEEP/{dev,deployed}.txt"
echo "  env.storage dev vs deployed: $PASS passed, $FAIL failed  (mutation: $MUTATE)  (floor $STORAGE_MIN_PASSED)"
echo "  MUTATIONS (diff half): MUTATE=no-storage-url and MUTATE=no-config must both turn this RED"
echo "  MUTATIONS (absolute half): MUTATE=text-no-ctype | list-prefix-broad | stream-corrupt"
echo "    -- each must leave the DIFF green and turn its own section-3b verdict RED"

rc=0
[ "$FAIL" -eq 0 ] || rc=1
if [ "$PASS" -lt "$STORAGE_MIN_PASSED" ]; then
  echo "FAIL: only $PASS assertions passed, fewer than the $STORAGE_MIN_PASSED this gate expects." >&2
  echo "      Assertions do not vanish by accident: either the deployed capture lost" >&2
  echo "      rows or an assertion was removed. If the removal was deliberate, lower" >&2
  echo "      STORAGE_MIN_PASSED in the same change and say why; do not treat the gap" >&2
  echo "      as slack." >&2
  rc=1
fi
exit "$rc"
