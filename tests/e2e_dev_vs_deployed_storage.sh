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
# ZEROSHIP_STORAGE_URL says otherwise), while a deployed worker runs whatever
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
#                           learned this with --kv-url.)
#   MUTATE=no-config        build without src/server/config.ts, so every
#                           procedure resolves to `auth: "user"` and the
#                           gateway refuses all fourteen -- green in dev, 401
#                           deployed. That is how kv-dashboard shipped (#163).
# Both must turn the diff RED. If they do not, this script is decorative.
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
APP="$ROOT/examples/storage-probe"
ZSHIP="$APP/dist/app.zship"
WORK="$(mktemp -d)"

PG_CONTAINER="${PG_CONTAINER:-compose-postgres-1}"
PG_USER="${PG_USER:-postgres}"
PG_DB="${PG_DB:-zeroship_storageleg}"
DB_URL="${DATABASE_URL:-postgres://postgres:zeroship@localhost:5440/$PG_DB}"
# A band of its own: kv uses 9392/8392/8302/3011, streaming 9394/8394/8304/3061,
# golden_path 9390/8390/8300 -- so all four can run at once.
CONTROL_PORT="${CONTROL_PORT:-9396}"
WORKER_PORT="${WORKER_PORT:-8396}"
GATE_PORT="${GATE_PORT:-8306}"
DEV_PORT="${DEV_PORT:-3081}"
MINIO_PORT="${MINIO_PORT:-9203}"
MINIO_CONTAINER="zs-devdeploy-storage-minio"
MINIO_ACCESS="minioadmin"; MINIO_SECRET="minioadmin"; MINIO_BUCKET="zeroship-storage-probe"
CONTROL_KEY="sp-ck"; MASTER_KEY="sp-mk"
export ZEROSHIP_DEV_INSECURE=1
export WORKER_KEY="${WORKER_KEY:-storageprobe-worker-key-0123456789ab}"
APP_NAME="storagep"
DEPLOYED_STORAGE="${DEPLOYED_STORAGE:-s3}"
MUTATE="${MUTATE:-none}"
# The worker's --storage-url also reads this variable, so an inherited value
# from the caller's shell would quietly become the deployed backend. Each side
# gets its backend explicitly below; nothing here may come from the ambient
# environment.
unset ZEROSHIP_STORAGE_URL

PASS=0; FAIL=0; PIDS=()
pass() { PASS=$((PASS+1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }
cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  lsof -ti :"$DEV_PORT" 2>/dev/null | xargs -r kill 2>/dev/null || true
  docker rm -f "$MINIO_CONTAINER" >/dev/null 2>&1 || true
  # Restore the config file if the no-config mutation moved it.
  [ -f "$WORK/config.ts.bak" ] && cp "$WORK/config.ts.bak" "$APP/src/server/config.ts"
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
# mtime is a weak proxy for provenance, so this warns loudly rather than
# failing: on a fresh clone every source file is newer than nothing, and the
# question it answers ("did you rebuild after touching these crates?") is worth
# a line of output even when the answer is fine.
# `src` only, and one `find -printf | sort` rather than `xargs ls -t`: the
# runtime crate carries a 1.1GB gitignored WPT checkout under tests/, and a
# multi-batch xargs would report the newest file of the LAST batch.
newest_src=$(find "$ROOT/crates/plugin-storage/src" "$ROOT/crates/runtime/src" \
  "$ROOT/crates/worker/src" "$ROOT/crates/gateway/src" "$ROOT/crates/control/src" \
  "$ROOT/libs/compio-s3/src" "$ROOT/sdks/storage/src" \
  \( -name '*.rs' -o -name '*.ts' \) -printf '%T@ %p\n' 2>/dev/null \
  | sort -nr | head -1 | cut -d' ' -f2-)
for b in zeroship zeroship-worker zeroship-gate zeroship-control dev-provision; do
  [ -x "$BIN/$b" ] || { fail "missing $BIN/$b -- see the prereqs in this file's header"; exit 2; }
  if [ -n "$newest_src" ] && [ "$newest_src" -nt "$BIN/$b" ]; then
    echo "  WARN $b is OLDER than $(basename "$newest_src") -- rebuild, or the diff below may be"
    echo "       reporting a version skew between the two sides rather than a real divergence."
  fi
done

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
# clap arg also reads ZEROSHIP_STORAGE_URL, so an exported value silently
# becomes the deployed backend's fallback: with it exported, MUTATE=no-storage-url
# booted a worker that quietly picked up the DEV directory and the mutation
# passed 11/0 -- a mutation that cannot fail proves nothing about the check it
# is meant to be testing.
mkdir -p "$WORK/dev-storage"
lsof -ti :"$DEV_PORT" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
( cd "$APP" && ZEROSHIP_STORAGE_URL="file://$WORK/dev-storage" \
    ./node_modules/.bin/vite > "$WORK/dev.log" 2>&1 ) & PIDS+=($!)
for _ in $(seq 1 20); do
  curl -sf -o /dev/null -m 2 -X POST -H 'content-type: application/json' \
    "http://localhost:$DEV_PORT/__zeroship/v1/probe.ping" -d '{"json":{}}' && break
  sleep 2
done
curl -sf -o /dev/null -m 5 -X POST -H 'content-type: application/json' \
  "http://localhost:$DEV_PORT/__zeroship/v1/probe.ping" -d '{"json":{}}' \
  && pass "dev app reachable" || { fail "dev app never came up"; tail -20 "$WORK/dev.log"; exit 1; }
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

for p in $CONTROL_PORT $WORKER_PORT $GATE_PORT; do lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "DROP DATABASE IF EXISTS $PG_DB WITH (FORCE)" >/dev/null 2>&1 || true
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "CREATE DATABASE $PG_DB" >/dev/null 2>&1 || true
"$BIN/zeroship-platform-migrate" --database-url "$DB_URL" --migrations-dir "$ROOT/db/migrations-ts" \
  --project-schema zeroship --project-id zeroship > "$WORK/migrate.log" 2>&1 \
  || { fail "platform migrations failed"; tail -20 "$WORK/migrate.log"; exit 1; }

"$BIN/zeroship-control" --port "$CONTROL_PORT" --db "$DB_URL" --blob-store "$WORK/bundles" \
  --control-key "$CONTROL_KEY" --master-key "$MASTER_KEY" > "$WORK/control.log" 2>&1 & PIDS+=($!)
sleep 4
# Without --storage-url the env.storage namespace is absent BY DESIGN
# (crates/worker/src/main.rs:386) and every handler fails loudly. Omitting it
# would look like an app bug rather than a harness bug -- which is exactly the
# MUTATE=no-storage-url case below.
STORAGE_FLAG=(--storage-url "$STORAGE_ARG")
[ "$MUTATE" = "no-storage-url" ] && { STORAGE_FLAG=(); echo "  (MUTATION: deployed worker booted with NO --storage-url)"; }
"$BIN/zeroship-worker" --port "$WORKER_PORT" --worker-threads 2 --control "http://localhost:$CONTROL_PORT" \
  --control-key "$CONTROL_KEY" --blob-store "$WORK/bundles" --poll-interval 2 \
  "${STORAGE_FLAG[@]}" > "$WORK/worker.log" 2>&1 & PIDS+=($!)
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
sleep 6   # gateway route-sync poll

curl -sf -o /dev/null -m 10 -X POST -H 'content-type: application/json' -H "X-Api-Key: $API_KEY" \
  "http://localhost:$GATE_PORT/apps/$APP_NAME/__zeroship/v1/probe.ping" -d '{"json":{}}' \
  && pass "deployed app reachable" || fail "deployed app did not answer ping"
probe "http://localhost:$GATE_PORT/apps/$APP_NAME" "X-Api-Key: $API_KEY" > "$WORK/deployed.txt" 2>&1
grep -q '"textMatches":true' "$WORK/deployed.txt" && pass "deployed side answered the probe" \
  || { fail "deployed side did not round-trip text"; head -20 "$WORK/deployed.txt"; tail -10 "$WORK/worker.log"; }

# --- 4. THE POINT: identical operations must produce identical results -----
if diff -q "$WORK/dev.txt" "$WORK/deployed.txt" >/dev/null 2>&1; then
  pass "dev and deployed results are identical across every probed operation"
else
  fail "dev and deployed DIVERGE -- results below (< dev, > deployed)"
  diff "$WORK/dev.txt" "$WORK/deployed.txt" | head -60
  echo ""
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
echo ""
echo "  results kept at $KEEP/{dev,deployed}.txt"
echo "  env.storage dev vs deployed: $PASS passed, $FAIL failed"
echo "  MUTATIONS: MUTATE=no-storage-url and MUTATE=no-config must both turn this RED"
[ "$FAIL" -eq 0 ]
