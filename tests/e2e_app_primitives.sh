#!/usr/bin/env bash
# ============================================================================
# e2e_app_primitives.sh — close ISS-54 / G1: exercise a REAL example app's
# primitives through the REAL multi-node edge (gateway → worker → V8).
#
# What this does, end to end, against a CLEAN ephemeral stack:
#   1. Stand up a fresh ephemeral Postgres + apply deploy/ops/postgres-init.sql +
#      the full platform migration set (0001→0036). A migration failure here is
#      a finding — the migration set must apply cleanly from scratch.
#   2. Boot control + migrated + worker + gateway with generated signing and
#      shared keys.
#   3. Mint an admin platform bearer OFFLINE (control's `/api/apps` now
#      requires a real platform OAuth access token - the old `--master-key`
#      bearer is gone). We stand up a one-key JWKS on the harness's own
#      signing key, name it as control's trusted issuer, seed a matching
#      `zeroship.users` row, and sign an `at+jwt`
#      with jose so control verifies it against that JWKS.
#   4. Create an app, deploy the built `examples/db-todos` .zship, and APPLY ITS
#      MIGRATIONS with `zeroship migrate`. The migrate step was absent until
#      2026-08-14 and its absence is what made stage 5's env.db arms red: the
#      app was deployed against a schema that had never been created, so the
#      per-app role the runtime `SET LOCAL ROLE`s to did not exist. The reds
#      were the harness faithfully reporting an unmigrated app, and the arm
#      below that blamed schema-init was reading the wrong cause.
#   5. Exercise primitives THROUGH THE EDGE:
#        a. anonymous SSR/HTML for a schema-less app  (proves dispatch+load+serve)
#        b. db-todos RPC over the gateway          (env.db primitive)
#        c. db-todos RPC direct to worker /dispatch (env.db, worker bearer)
#
# Known findings this harness SURFACES (see the report at the end):
#   * SEC-5 fail-closed default — db-todos RPC procedures declare no `auth`,
#     so the gateway defaults them to `User` ⇒ 401 without a real OIDC (native
#     OP) session. There is no shortcut to mint an app session,
#     so authenticated env.db RPC through the gateway is not headlessly
#     reachable without standing up the native OP. (gateway-auth gap)
#   * Schema adapter resolution is owned by DbPlugin. It supplies its compiled
#     host module independently of creator bundling.
#     Exercise that delivery with the native plugin and its PostgreSQL fixture:
#       cargo test -p zeroship-data-v8 --lib tests::postgres::plugin_modules
#
# So db-todos DOES run on the worker. It is the DEPLOYED vehicle of
# tests/e2e_dev_vs_deployed_db.sh, and `tests/golden_path.sh` step 10 deploys
# an env.db app and drives it over the gateway. Any KNOWN-FAIL arm below whose
# stated reason is the SCHEMA-INIT bug is therefore justified by something that
# is no longer true, and needs re-checking against a live run rather than being
# trusted — see task #256, which tracks that this harness's `known()` counter
# conflates four incompatible meanings and does not fail at the default
# STRICT=0. Set STRICT=1 to make the known-fails hard-fail.
#
# Usage:
#   ./tests/e2e_app_primitives.sh
#   STRICT=1 ./tests/e2e_app_primitives.sh   # known-fails become failures
#
# Requires: docker, jq-free (uses node), node (+ workspace jose), openssl,
#           a release build (target/release/{zeroship,zeroship-control,
#           zeroship-gate,zeroship-worker}), and a built examples/db-todos
#           (cd examples/db-todos && pnpm install && pnpm build).
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release"
# shellcheck source=tests/lib/runtime_secrets.sh
source "$ROOT/tests/lib/runtime_secrets.sh"
# `zeroship.app_members` is deleted; an app reaches the people who answer for it
# through its project's organization. `seat_app_owner` writes that join, reads
# the seat back out of the database and exits when it is not there - an
# INSERT ... SELECT over no rows is a SUCCESSFUL statement that seats nobody,
# and the 403 it later produces surfaces far from here.
source "$ROOT/tests/lib/organization_fixture.sh"
STRICT="${STRICT:-0}"

# --- ports (offset from e2e_platform.sh to avoid colliding with a dev stack)
ZEROSHIP_CONTROL_PORT=9099
ZEROSHIP_WORKER_PORT=8087
ZEROSHIP_GATEWAY_PORT=8001
ZEROSHIP_MIGRATE_SERVER_PORT=9098
PG_PORT=5443
PG_CONTAINER="zs-e2e-pg"

# --- jose ESM entry in the workspace pnpm store (no direct node_modules link)
JOSE_JS="$ROOT/node_modules/.pnpm/jose@6.2.3/node_modules/jose/dist/webapi/index.js"

PASS=0; FAIL=0; KNOWN=0
PIDS=()
WORK=""

pass()  { PASS=$((PASS+1));  echo "  ✓ $1"; }
fail()  { FAIL=$((FAIL+1));  echo "  ✗ $1"; }
known() { KNOWN=$((KNOWN+1)); echo "  ⚠ KNOWN-FAIL: $1"; if [ "$STRICT" = "1" ]; then FAIL=$((FAIL+1)); fi; }

cleanup() {
  echo ""
  echo "=== Cleanup ==="
  for pid in "${PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
  wait 2>/dev/null || true
  docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
  # Keep the logs when anything failed. This trap used to remove $WORK
  # unconditionally, so a run destroyed worker.log in the same breath as the
  # failure it recorded - the response body is the generic production error, so
  # the only copy of the real cause went with it.
  if [ -n "$WORK" ]; then
    if [ "${FAIL:-0}" -gt 0 ] || [ "${KNOWN:-0}" -gt 0 ] || [ "${KEEP_LOGS:-0}" = "1" ]; then
      echo "  logs kept at $WORK (control.log/worker.log/gate.log)"
    else
      rm -rf "$WORK"
    fi
  fi
  echo "  stack down, ephemeral PG removed"
}
trap cleanup EXIT

# node helper: read a JSON field from stdin
jget() { node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{try{const o=JSON.parse(s);process.stdout.write(String(o$1??'')+'\n')}catch(e){console.log('')}})"; }

echo "============================================"
echo "  zeroship E2E — app primitives over the edge (ISS-54/G1)"
echo "============================================"

# --- preflight -------------------------------------------------------------
for b in zeroship zeroship-control zeroship-gate zeroship-worker zeroship-migrate-server; do
  [ -x "$BIN/$b" ] || { echo "missing $BIN/$b - run cargo build --release"; exit 2; }
done
[ -f "$ROOT/packages/zero-migrate-cli/dist/cli-bin.js" ] || { echo "missing the zero-migrate CLI - run: pnpm install && pnpm build"; exit 2; }
ZSHIP="$ROOT/examples/db-todos/dist/app.zship"
[ -f "$ZSHIP" ] || { echo "missing $ZSHIP — run: (cd examples/db-todos && pnpm install && pnpm build)"; exit 2; }
[ -f "$JOSE_JS" ] || { echo "missing jose at $JOSE_JS"; exit 2; }
command -v docker >/dev/null || { echo "docker required"; exit 2; }
command -v openssl >/dev/null || { echo "openssl required"; exit 2; }

WORK="$(mktemp -d -t zs-e2e-prim-XXXXXX)"
mkdir -p "$WORK/blobs" "$WORK/blob-cache"

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 1: ephemeral Postgres + migrations (0001→0036) ==="
docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
docker run --name "$PG_CONTAINER" -d -p "$PG_PORT:5432" \
  -e POSTGRES_PASSWORD=zeroship -e POSTGRES_USER=postgres -e POSTGRES_DB=zeroship \
  postgres:16 -c max_connections=300 >/dev/null
for i in $(seq 1 30); do
  if docker logs "$PG_CONTAINER" 2>&1 | grep -Fq 'PostgreSQL init process complete' \
    && docker exec "$PG_CONTAINER" pg_isready -U postgres >/dev/null 2>&1; then
    break
  fi
  sleep 1
done
docker exec "$PG_CONTAINER" pg_isready -U postgres >/dev/null 2>&1 && pass "ephemeral PG ready on :$PG_PORT" || { fail "PG never became ready"; exit 1; }

if [ -f "$ROOT/deploy/ops/postgres-init.sql" ]; then
  docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 < "$ROOT/deploy/ops/postgres-init.sql" >/dev/null 2>&1 \
    && pass "applied deploy/ops/postgres-init.sql" || fail "postgres-init.sql failed"
fi

DBURL="postgres://postgres:zeroship@localhost:$PG_PORT/zeroship"
MIG_LOG="$WORK/migrate.log"
if zs_platform_migrate "$DBURL" \
    --migrations-dir "$ROOT/db/migrations-ts" \
    --project-schema zeroship --project-id zeroship > "$MIG_LOG" 2>&1; then
  pass "platform migrations applied cleanly from scratch (zeroship-platform-migrate)"
else
  fail "zeroship-platform-migrate FAILED (see $MIG_LOG)"; tail -20 "$MIG_LOG"; exit 1
fi

# sanity: control tables + apps.system column
psql_q() { docker exec "$PG_CONTAINER" psql -U postgres -d zeroship -tAc "$1" 2>/dev/null | tr -d '[:space:]'; }
[ "$(psql_q "select to_regclass('zeroship.apps') is not null")" = "t" ] && pass "zeroship.apps exists" || fail "zeroship.apps missing"
[ "$(psql_q "select count(*) from information_schema.columns where table_schema='zeroship' and table_name='apps' and column_name='system'")" = "1" ] \
  && pass "apps.system column present (0036)" || fail "apps.system column missing"

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 2: boot authenticated stack ==="

# stable ed25519 PKCS#8 signing key: the harness's own platform OP publishes it
# as a one-key JWKS and mints admin bearers with it, and the gateway loads it
# as its wrapper-token signing key.
openssl genpkey -algorithm ed25519 -out "$WORK/signing-key.pem" 2>/dev/null
chmod 600 "$WORK/signing-key.pem"

for p in $ZEROSHIP_CONTROL_PORT $ZEROSHIP_WORKER_PORT $ZEROSHIP_GATEWAY_PORT $ZEROSHIP_MIGRATE_SERVER_PORT; do lsof -ti :$p 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done

ZEROSHIP_GATEWAY_SIGNING_KEY_FILE="$WORK/signing-key.pem"
# The issuer control verifies the admin bearer against, on the same key the
# gateway signs with. Up BEFORE control: control reads the issuer once at boot.
e2e_platform_op_up "$WORK/signing-key.pem" "$WORK" || exit 1
PIDS+=($E2E_PLATFORM_OP_PID)
ZEROSHIP_GATEWAY_BROKER_SECRET_FILE="$WORK/gate-secret"
e2e_export_runtime_secrets "$WORK" || exit 1
e2e_export_database_urls "$DBURL"
"$BIN/zeroship-control" --port $ZEROSHIP_CONTROL_PORT \
  --blob-store "$WORK/blobs" \
 > "$WORK/control.log" 2>&1 &
PIDS+=($!)
for i in $(seq 1 30); do curl -sf "http://localhost:$ZEROSHIP_CONTROL_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$ZEROSHIP_CONTROL_PORT/readyz" >/dev/null 2>&1 && pass "control healthy" || { fail "control unhealthy"; tail -20 "$WORK/control.log"; exit 1; }

# The migration service. It was ABSENT from this harness, which is why stage 5's
# env.db arms were red: the app was deployed and never migrated, so the per-app
# role the runtime SET LOCAL ROLEs to had never been created. That is a missing
# step in the harness, not a defect in the app or the runtime, and no amount of
# re-reading the worker log was going to say so - the error it prints
# (`role "app_..._role" does not exist`) names a role whose only producer is
# this service. It takes no signing key of its own: the service verifies the
# creator's bearer and authorizes the apply itself.
"$BIN/zeroship-migrate-server" --port $ZEROSHIP_MIGRATE_SERVER_PORT \
  --tmp-dir "$WORK/migrated-tmp" \
 > "$WORK/migrated.log" 2>&1 &
PIDS+=($!)
for i in $(seq 1 30); do curl -sf "http://localhost:$ZEROSHIP_MIGRATE_SERVER_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$ZEROSHIP_MIGRATE_SERVER_PORT/readyz" >/dev/null 2>&1 && pass "migrated healthy" || { fail "migrated unhealthy"; tail -20 "$WORK/migrated.log"; exit 1; }

e2e_start_cdc_relay "$BIN/zeroship-data-cdc-server" || exit 1
# worker: generated worker_key; direct /dispatch calls present its bearer;
# shared blob-store with control (single-host shared-volume pattern);
# ZEROSHIP_WORKER_DATABASE_URL for env.db.
# Keep the diagnostic probes on one warmed isolate. Separate worker threads can
# select a less-warmed isolate whose free-tier 50 ms CPU budget expires after
# the database error is classified but before the SSE frame reaches the client.
# That tests cold-start limits rather than the error payload.
"$BIN/zeroship-worker" --port $ZEROSHIP_WORKER_PORT --threads 1 \
  --control-url "http://localhost:$ZEROSHIP_CONTROL_PORT" \
  --blob-store "$WORK/blobs" --poll-interval 2 > "$WORK/worker.log" 2>&1 &
PIDS+=($!)
for i in $(seq 1 30); do curl -sf "http://localhost:$ZEROSHIP_WORKER_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$ZEROSHIP_WORKER_PORT/readyz" >/dev/null 2>&1 && pass "worker healthy" || { fail "worker unhealthy"; tail -20 "$WORK/worker.log"; exit 1; }

# Broker master secret. cd54028e7 made the gateway refuse to start without one,
# and this harness was never updated, so it has been unable to get past "gateway
# healthy" since.
openssl rand -base64 48 > "$WORK/gate-secret"
chmod 600 "$WORK/gate-secret"

"$BIN/zeroship-gate" --port $ZEROSHIP_GATEWAY_PORT --control-url "http://localhost:$ZEROSHIP_CONTROL_PORT" \
  --worker-urls "http://localhost:$ZEROSHIP_WORKER_PORT" --blob-store "$WORK/blobs" \
  --blob-cache-disk-root "$WORK/blob-cache" --poll-interval 2 \
  --broker-secret-file "$WORK/gate-secret" \
 > "$WORK/gate.log" 2>&1 &
PIDS+=($!)
for i in $(seq 1 30); do curl -sf "http://localhost:$ZEROSHIP_GATEWAY_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$ZEROSHIP_GATEWAY_PORT/readyz" >/dev/null 2>&1 && pass "gateway healthy" || { fail "gateway unhealthy"; tail -20 "$WORK/gate.log"; exit 1; }

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 3: mint admin platform bearer (offline) ==="
# The scope string is the deleted permission_tokens policy's action list,
# one-for-one: it becomes the token policy control intersects with the
# owner's own authority (TOKEN and USER).
SCOPE="apps:read apps:write apps:deploy apps:archive deployments:read env:read env:write secrets:read secrets:write"

OWNER="$(node -e 'console.log(require("crypto").randomUUID())')"

docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.users (id, email, name, email_verified_at)
VALUES ('$OWNER', 'e2e-$OWNER@zeroship.test'::citext, 'E2E Admin', NOW());
SQL

ADMIN_TOKEN="$(e2e_mint_platform_bearer "$OWNER" "$SCOPE")"
[ "$(echo -n "$ADMIN_TOKEN" | awk -F. '{print NF}')" = "3" ] && pass "minted platform bearer" || { fail "bearer mint failed: $ADMIN_TOKEN"; exit 1; }

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 4: create app + deploy db-todos ==="
APP_JSON="$(curl -s -X POST "http://localhost:$ZEROSHIP_CONTROL_PORT/api/apps" \
  -H 'Content-Type: application/json' -H "Authorization: Bearer $ADMIN_TOKEN" -d '{"name":"db-todos-e2e"}')"
APP_ID="$(echo "$APP_JSON" | jget '.id')"
[ -n "$APP_ID" ] && pass "created app db-todos-e2e ($APP_ID)" || { fail "create app: $APP_JSON"; exit 1; }

DEP="$("$BIN/zeroship" deploy "$ZSHIP" --app="$APP_ID" --control="http://localhost:$ZEROSHIP_CONTROL_PORT" --token="$ADMIN_TOKEN" 2>&1)"
echo "$DEP" | grep -q "deploy_hash" && pass "deployed db-todos .zship" || fail "deploy failed: $DEP"

# APP OWNERSHIP, and this row is NOT redundant with the platform-admin grant
# above. The two services differ: control's deploy handler stops at the Cedar
# decision (crates/zeroship-control/src/authz_guard.rs), which `admin.cedar` satisfies on
# its own, while migrated ALSO requires a literal `role = 'owner'` row
# (crates/zeroship-migrate-server/src/auth.rs, `requires_app_owner`). A platform admin with no
# membership row can therefore deploy an app and be refused when migrating it.
seat_app_owner "$APP_ID" "$OWNER" owner \
  docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -v ON_ERROR_STOP=1

# Build a worker dispatch frame for the pre-migrate diagnostic probes.
zs_unmigrated_frame() {
  node -e '
const fs = require("fs");
const [out, method, url, body, accept] = process.argv.slice(1);
const headers = [["content-type", "application/json"]];
if (accept) headers.push(["accept", accept]);
const meta = Buffer.from(JSON.stringify({ method, url, headers }), "utf8");
const len = Buffer.alloc(4);
len.writeUInt32LE(meta.length, 0);
fs.writeFileSync(out, Buffer.concat([len, meta, Buffer.from(body, "utf8")]));
' "$@"
}

echo ""
echo "=== Stage 4b: unmigrated app diagnostics ==="
APP_ROLE="app_${APP_ID}_role"
if [ "$(psql_q "select count(*) from pg_roles where rolname='$APP_ROLE'")" != "0" ]; then
  fail "precondition failed: $APP_ROLE already exists"
  exit 1
fi
sleep 4

zs_unmigrated_frame "$WORK/frame-unmigrated-auto.bin" POST \
  "http://db-todos-e2e.localhost/__zeroship/v1/users.public" '{"json":{}}'
AUTO_RESP="$(curl -sS -w '\n%{http_code}' -X POST \
  "http://localhost:$ZEROSHIP_WORKER_PORT/dispatch/$APP_ID" \
  -H "Authorization: Bearer $E2E_STALE_WORKER_BEARER" \
  -H 'content-type: application/octet-stream' \
  --data-binary @"$WORK/frame-unmigrated-auto.bin")"
AUTO_CODE="$(echo "$AUTO_RESP" | tail -1)"
AUTO_BODY="$(echo "$AUTO_RESP" | sed '$d')"
if [ "$AUTO_CODE" = "500" ] \
  && grep -Eq '"code":"(schema_not_provisioned|SCHEMA_NOT_PROVISIONED)"' <<<"$AUTO_BODY" \
  && grep -Fq 'zeroship migrate' <<<"$AUTO_BODY"; then
  pass "unmigrated autocommit response names zeroship migrate"
else
  fail "unmigrated autocommit response lost remediation: HTTP $AUTO_CODE body=$AUTO_BODY"
fi

zs_unmigrated_frame "$WORK/frame-unmigrated-tx.bin" POST \
  "http://db-todos-e2e.localhost/__zeroship/v1/diagnostics.unmigratedTransaction" \
  '{"json":null}'
TX_RESP="$(curl -sS -w '\n%{http_code}' -X POST \
  "http://localhost:$ZEROSHIP_WORKER_PORT/dispatch/$APP_ID" \
  -H "Authorization: Bearer $E2E_STALE_WORKER_BEARER" \
  -H 'content-type: application/octet-stream' \
  --data-binary @"$WORK/frame-unmigrated-tx.bin")"
TX_CODE="$(echo "$TX_RESP" | tail -1)"
TX_BODY="$(echo "$TX_RESP" | sed '$d')"
if [ "$TX_CODE" = "500" ] \
  && grep -Eq '"code":"(schema_not_provisioned|SCHEMA_NOT_PROVISIONED)"' <<<"$TX_BODY" \
  && grep -Fq 'zeroship migrate' <<<"$TX_BODY"; then
  pass "unmigrated transaction response names zeroship migrate"
else
  fail "unmigrated transaction response lost remediation: HTTP $TX_CODE body=$TX_BODY"
fi

zs_unmigrated_frame "$WORK/frame-unmigrated-stream.bin" POST \
  "http://db-todos-e2e.localhost/__zeroship/v1/diagnostics.unmigratedStream" \
  '{"json":null}' 'text/event-stream'
STREAM_RESP="$(curl -s -N --max-time 5 -w '\n%{http_code}' -X POST \
  "http://localhost:$ZEROSHIP_WORKER_PORT/dispatch/$APP_ID" \
  -H "Authorization: Bearer $E2E_STALE_WORKER_BEARER" \
  -H 'content-type: application/octet-stream' \
  --data-binary @"$WORK/frame-unmigrated-stream.bin")"
STREAM_CODE="$(echo "$STREAM_RESP" | tail -1)"
STREAM_BODY="$(echo "$STREAM_RESP" | sed '$d')"
if [ "$STREAM_CODE" = "200" ] \
  && grep -Eq '"code":"(schema_not_provisioned|SCHEMA_NOT_PROVISIONED)"' <<<"$STREAM_BODY" \
  && grep -Fq 'zeroship migrate' <<<"$STREAM_BODY"; then
  pass "unmigrated streaming response names zeroship migrate"
else
  fail "unmigrated streaming response lost remediation: HTTP $STREAM_CODE body=$STREAM_BODY"
fi

# The three diagnostics above intentionally ran before the database existed.
# Create it explicitly only now, then apply migrations as a separate operation.
# This harness has no edge, so both requests go directly to migrate-server's
# loopback URL.
CREATE_CODE="$(curl -sS -o "$WORK/create-database-response.json" -w '%{http_code}' -X POST \
  "http://localhost:$ZEROSHIP_MIGRATE_SERVER_PORT/v1/databases/$APP_ID" \
  -H "Authorization: Bearer $ADMIN_TOKEN")"
if [[ "$CREATE_CODE" != 2?? ]]; then
  fail "database create failed (http=$CREATE_CODE): $(cat "$WORK/create-database-response.json")"
  tail -20 "$WORK/migrated.log"
  exit 1
fi

IR_JSON="$ROOT/examples/db-todos/generated/zeroship/migrations.ir.json"
[ -f "$IR_JSON" ] || { echo "missing $IR_JSON — run: pnpm gen-types"; exit 2; }
MIG="$("$BIN/zeroship" migrate "$IR_JSON" --app="$APP_ID" \
  --control="http://localhost:$ZEROSHIP_MIGRATE_SERVER_PORT" --token="$ADMIN_TOKEN" 2>&1)"
if grep -qE 'Applied [1-9][0-9]* migration op' <<<"$MIG"; then
  pass "zeroship migrate applied db-todos' schema through migrate-server ($(head -c 60 <<<"$MIG"))"
else
  fail "zeroship migrate failed: $MIG"
  tail -20 "$WORK/migrated.log"
fi
sleep 4   # let route + version sync to gateway + worker

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 5a: schema-LESS app over the real edge (control) ==="
# Proves routing + on-demand load + V8 fetch end-to-end, isolating the
# schema-init bug from the rest of the dispatch path.
STAGE="$(mktemp -d)"; mkdir -p "$STAGE/blobs"
cat > "$STAGE/app.js" <<'JS'
export default { fetch() { return new Response(JSON.stringify({ ok: true }), { status: 200, headers: { "content-type": "application/json" } }); } };
JS
H="$(sha256sum "$STAGE/app.js" | awk '{print $1}')"
cp "$STAGE/app.js" "$STAGE/blobs/$H"
cat > "$STAGE/manifest.json" <<EOF
{"version":1,"resources":{"/[...rest]":{"auth":"anonymous","publicly_accessible":true}},"assets":{},"runtime_assets":{},"asset_version":0,"sourcemaps":{},"worker":{"entry":"index.js","modules":{"index.js":"$H"}},"metadata":{"compiler":"e2e","built_at":"$(date -u +%FT%TZ)"}}
EOF
(cd "$STAGE" && tar --format=ustar -cf - manifest.json "blobs/$H") | zstd -q -f -o "$STAGE/app.zship"
NS_JSON="$(curl -s -X POST "http://localhost:$ZEROSHIP_CONTROL_PORT/api/apps" -H 'Content-Type: application/json' -H "Authorization: Bearer $ADMIN_TOKEN" -d '{"name":"noschema-e2e"}')"
NS_ID="$(echo "$NS_JSON" | jget '.id')"
"$BIN/zeroship" deploy "$STAGE/app.zship" --app="$NS_ID" --control="http://localhost:$ZEROSHIP_CONTROL_PORT" --token="$ADMIN_TOKEN" >/dev/null 2>&1
rm -rf "$STAGE"
sleep 4
NS_RESP="$(curl -s -w '\n%{http_code}' -H 'Host: noschema-e2e.localhost' "http://localhost:$ZEROSHIP_GATEWAY_PORT/")"
NS_CODE="$(echo "$NS_RESP" | tail -1)"
NS_BODY="$(echo "$NS_RESP" | head -1)"
if [ "$NS_CODE" = "200" ] && echo "$NS_BODY" | grep -q '"ok":true'; then
  pass "schema-less app served HTTP 200 through gateway→worker→V8"
else
  fail "schema-less app over edge: HTTP $NS_CODE body=$NS_BODY"
fi

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 5b: db-todos RPC through the GATEWAY (env.db over the edge) ==="
# SEC-5: db-todos RPC procedures declare no `auth`, so the gateway defaults
# them to `User` ⇒ 401 without a real OIDC session. Document the gate.
GW_RESP="$(curl -s -w '\n%{http_code}' -X POST \
  -H 'Host: db-todos-e2e.localhost' -H 'content-type: application/json' \
  "http://localhost:$ZEROSHIP_GATEWAY_PORT/__zeroship/v1/users.public" -d '{"json":{}}')"
GW_CODE="$(echo "$GW_RESP" | tail -1)"
GW_BODY="$(echo "$GW_RESP" | head -1)"
if [ "$GW_CODE" = "200" ] && echo "$GW_BODY" | grep -q '"json"'; then
  pass "db-todos users.public via gateway returned a row"
elif [ "$GW_CODE" = "401" ]; then
  known "gateway gates db-todos RPC at 401 (SEC-5 fail-closed default; no headless app session without the native OP). body=$GW_BODY"
else
  fail "db-todos RPC via gateway: HTTP $GW_CODE body=$GW_BODY"
fi

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 5c: db-todos RPC DIRECT to worker /dispatch (authenticated) ==="
# Bypasses the gateway auth gate (the worker verifies a service assertion, so
# unauthenticated). This is the cleanest proof env.db works on the runtime —
# IF schema-init succeeds.
# zs_frame <out> <method> <url> <body> — write the dispatch frame the worker
# actually decodes.
#
# /dispatch takes a BINARY frame (crates/zeroship-core/src/dispatch_frame.rs
# encode_dispatch_frame): a 4-byte little-endian metadata length, then the
# {method,url,headers} JSON, then the raw body bytes. The body is NOT a field
# of the metadata.
#
# This harness used to POST a single JSON object with the body inline. The
# decoder read its first four bytes — `{"me` — as the length prefix, which is
# 1,701,651,067 against a 64 KiB cap, and answered
# `invalid envelope: dispatch metadata too large`. Every Stage 5c run failed
# there, and the failure was reported as "schema-init fails => env.db
# unreachable", which is a label this script prints regardless of cause. env.db
# was never reached, so nothing below has ever been evidence about it either way.
zs_frame() {
  node -e '
const fs = require("fs");
const [out, method, url, body] = process.argv.slice(1);
const meta = Buffer.from(JSON.stringify({
  method, url, headers: [["content-type", "application/json"]],
}), "utf8");
const len = Buffer.alloc(4);
len.writeUInt32LE(meta.length, 0);
fs.writeFileSync(out, Buffer.concat([len, meta, Buffer.from(body, "utf8")]));
' "$@"
}

zs_frame "$WORK/frame-users.bin" POST \
  "http://db-todos-e2e.localhost/__zeroship/v1/users.public" '{"json":{}}'
WK_RESP="$(curl -s -w '\n%{http_code}' -X POST "http://localhost:$ZEROSHIP_WORKER_PORT/dispatch/$APP_ID" -H "Authorization: Bearer $E2E_STALE_WORKER_BEARER" -H 'content-type: application/octet-stream' --data-binary @"$WORK/frame-users.bin")"
WK_CODE="$(echo "$WK_RESP" | tail -1)"
WK_BODY="$(echo "$WK_RESP" | head -1)"
if [ "$WK_CODE" = "200" ] && echo "$WK_BODY" | grep -q '"json"'; then
  pass "db-todos users.public via worker /dispatch returned a row (env.db works over the runtime)"
  # Follow up: a mutation + query to fully exercise env.db CRUD.
  UID_VAL="$(echo "$WK_BODY" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const o=JSON.parse(s);console.log((o.json&&o.json.id)||"")}catch(e){console.log("")}})')"
  if [ -n "$UID_VAL" ]; then
    zs_frame "$WORK/frame-create.bin" POST "http://x/__zeroship/v1/todos.create" \
      "$(node -e 'process.stdout.write(JSON.stringify({json:{userId:process.argv[1],title:"e2e todo"}}))' "$UID_VAL")"
    C_RESP="$(curl -s -X POST "http://localhost:$ZEROSHIP_WORKER_PORT/dispatch/$APP_ID" -H "Authorization: Bearer $E2E_STALE_WORKER_BEARER" -H 'content-type: application/octet-stream' --data-binary @"$WORK/frame-create.bin")"
    echo "$C_RESP" | grep -q '"json"' && pass "env.db mutation todos.create over worker" || fail "todos.create failed: $C_RESP"
    zs_frame "$WORK/frame-list.bin" POST "http://x/__zeroship/v1/todos.list" \
      "$(node -e 'process.stdout.write(JSON.stringify({json:{userId:process.argv[1]}}))' "$UID_VAL")"
    L_RESP="$(curl -s -X POST "http://localhost:$ZEROSHIP_WORKER_PORT/dispatch/$APP_ID" -H "Authorization: Bearer $E2E_STALE_WORKER_BEARER" -H 'content-type: application/octet-stream' --data-binary @"$WORK/frame-list.bin")"
    echo "$L_RESP" | grep -q 'e2e todo' && pass "env.db query todos.list returned the inserted row" || fail "todos.list missing row: $L_RESP"
  fi
else
  # Read the cause out of the log rather than asserting one. This arm used to
  # be hard-labelled "schema-init fails", which was a diagnosis printed
  # regardless of what actually happened - and for a long time the real cause
  # was a missing migrate step, not schema init. Check the role first, because
  # it is the failure a creator hits.
  ERR="$(grep -iEo 'role "app_[^"]*" does not exist' "$WORK/worker.log" | tail -1)"
  [ -n "$ERR" ] && ERR="$ERR (migrations were not applied to this app)"
  [ -z "$ERR" ] && ERR="$(grep -iEo "Cannot find module '[^']*'" "$WORK/worker.log" | tail -1)"
  [ -z "$ERR" ] && ERR="$(grep -iE 'install-schema|Evaluate rejected' "$WORK/worker.log" | tail -1)"
  fail "db-todos env.db unreachable over the worker. HTTP $WK_CODE; runtime error: ${ERR:-$WK_BODY}"
fi

# ---------------------------------------------------------------------------
echo ""
echo "============================================"
echo "  Results: $PASS passed, $FAIL failed, $KNOWN known-fail"
echo "============================================"
[ $FAIL -eq 0 ] && exit 0 || exit 1
