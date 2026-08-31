#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# END TO END: a creator project whose deploy target lives in `zeroship.jsonc`,
# with NO `--app` and NO `--control` typed anywhere.
#
#     cd <project>
#     zeroship migrate      # app + control + migrations.out from the file
#     zeroship deploy       # app + control + build.output   from the file
#
# and then a DATABASE-BACKED RPC returning real rows through the runtime, which
# is what proves the migrate step actually landed: `zeroship migrate` is the
# only producer of the per-app role the runtime `SET LOCAL ROLE`s to, so an
# env.db call that returns data is a receipt for the whole chain.
#
# THE REGRESSION PROBE MATTERS AS MUCH AS THE GREEN PATH. This harness proves
# both halves of the fix on one project: config-driven migration-path resolution
# succeeds, while the removed hardcoded path fails. Previously, the CLI used
# `DEFAULT_IR_PATH = "generated/zeroship/migrations.ir.json"` when no positional
# path was given, so a project whose build wrote anywhere else got a
# `failed to read` from a path it never chose. Same project, same command, one
# variable.
#
# WHAT THIS DOES NOT PROVE:
#   - It does not exercise `--env=`; the environments overlay is covered by the
#     Rust unit tests and by `tests/project_config_gate.sh`, not here.
#   - It does not repeat the writeback. `tests/project_config_gate.sh` has a
#     focused real-CLI first-deploy probe with an isolated curl stub, then runs
#     both readers on the file it wrote. This expensive harness keeps its app
#     pre-created so it can focus on real migration and database-backed RPC.
#   - The RPC is driven at the worker's `/dispatch`, not through the gateway.
#     db-todos' procedures declare no `auth`, so the gateway fail-closes them at
#     401 without an OIDC session - the same documented gate
#     `tests/e2e_app_primitives.sh` records. This harness is about the CONFIG
#     path, and routing the RPC through a login it does not need would only add
#     ways for it to fail for unrelated reasons.
#
# Requires: docker, openssl, node, and a release build including
#   cargo build --release
#   pnpm install && pnpm build && pnpm --filter zero-migrate-cli build
# plus a built examples/db-todos (`pnpm --filter db-todos build` or `pnpm build`).
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release"
JOSE_JS="$ROOT/node_modules/.pnpm/jose@6.2.3/node_modules/jose/dist/webapi/index.js"

# Own port band + container name so this can run beside the other harnesses.
: "${ZEROSHIP_CONTROL_PORT:=9131}"
: "${ZEROSHIP_EDGE_PORT:=9130}"
: "${ZEROSHIP_WORKER_PORT:=8091}"
: "${ZEROSHIP_GATEWAY_PORT:=8021}"
: "${ZEROSHIP_MIGRATE_SERVER_PORT:=9231}"
# 5461 was taken by an unrelated container on the machine this was written on.
# The band below is checked free; every value is overridable.
: "${PG_PORT:=5471}"
: "${PG_CONTAINER:=zs-e2e-projcfg-pg}"

PASS=0; FAIL=0; PIDS=()
pass() { PASS=$((PASS+1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }
jget() { node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{try{const o=JSON.parse(s);process.stdout.write(String(o$1??'')+'\n')}catch(e){console.log('')}})"; }

source "$ROOT/tests/lib/runtime_secrets.sh"

cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  # Only the container THIS script created.
  docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
  [ -n "${WORK:-}" ] && rm -rf "$WORK"
}
trap cleanup EXIT

echo "============================================"
echo "  zeroship E2E - deploy and migrate from zeroship.jsonc"
echo "============================================"

for b in zeroship zeroship-control zeroship-gate zeroship-worker zeroship-migrate-server; do
  [ -x "$BIN/$b" ] || { echo "missing $BIN/$b - run cargo build --release"; exit 2; }
done
[ -f "$ROOT/packages/zero-migrate-cli/dist/cli-bin.js" ] || { echo "missing the zero-migrate CLI - run: pnpm install && pnpm build && pnpm --filter zero-migrate-cli build"; exit 2; }
SRC_APP="$ROOT/examples/db-todos"
[ -f "$SRC_APP/dist/app.zship" ] || { echo "missing $SRC_APP/dist/app.zship - run: (cd $SRC_APP && pnpm build)"; exit 2; }
[ -f "$JOSE_JS" ] || { echo "missing jose at $JOSE_JS"; exit 2; }
command -v docker >/dev/null || { echo "docker required"; exit 2; }
command -v openssl >/dev/null || { echo "openssl required"; exit 2; }

WORK="$(mktemp -d -t zs-e2e-projcfg-XXXXXX)"
mkdir -p "$WORK/blobs" "$WORK/blob-cache"

# ---------------------------------------------------------------------------
echo ""
echo "=== prepare Postgres and apply platform migrations ==="
docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
docker run --name "$PG_CONTAINER" -d -p "$PG_PORT:5432" \
  -e POSTGRES_PASSWORD=zeroship -e POSTGRES_USER=postgres -e POSTGRES_DB=zeroship \
  postgres:16 -c max_connections=300 >/dev/null
for _ in $(seq 1 30); do docker exec "$PG_CONTAINER" pg_isready -U postgres >/dev/null 2>&1 && break; sleep 1; done
docker exec "$PG_CONTAINER" pg_isready -U postgres >/dev/null 2>&1 \
  && pass "ephemeral PG ready on :$PG_PORT" || { fail "PG never became ready"; exit 1; }

[ -f "$ROOT/deploy/ops/postgres-init.sql" ] && \
  docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 \
    < "$ROOT/deploy/ops/postgres-init.sql" >/dev/null 2>&1

DBURL="postgres://postgres:zeroship@localhost:$PG_PORT/zeroship"
if zs_platform_migrate "$DBURL" \
    --migrations-dir "$ROOT/db/migrations-ts" \
    --project-schema zeroship --project-id zeroship > "$WORK/platmig.log" 2>&1; then
  pass "platform migrations applied from scratch"
else
  fail "zeroship-platform-migrate FAILED"; tail -20 "$WORK/platmig.log"; exit 1
fi

# ---------------------------------------------------------------------------
echo ""
echo "=== start control, migrated, worker, and gateway ==="
openssl genpkey -algorithm ed25519 -out "$WORK/signing-key.pem" 2>/dev/null
chmod 600 "$WORK/signing-key.pem"
openssl rand -base64 48 > "$WORK/gate-secret"; chmod 600 "$WORK/gate-secret"
for p in $ZEROSHIP_EDGE_PORT $ZEROSHIP_CONTROL_PORT $ZEROSHIP_WORKER_PORT $ZEROSHIP_GATEWAY_PORT $ZEROSHIP_MIGRATE_SERVER_PORT; do
  lsof -ti :$p 2>/dev/null | xargs -r kill -9 2>/dev/null || true
done

ZEROSHIP_GATEWAY_SIGNING_KEY_FILE="$WORK/signing-key.pem"
# The issuer control verifies the admin bearer against, on the same key the
# gateway signs with. Up BEFORE control: control reads the issuer once at boot.
e2e_platform_op_up "$WORK/signing-key.pem" "$WORK" || exit 1
PIDS+=($E2E_PLATFORM_OP_PID)
ZEROSHIP_GATEWAY_BROKER_SECRET_FILE="$WORK/gate-secret"
e2e_export_runtime_secrets "$WORK" || exit 1
e2e_export_database_urls "$DBURL"

boot() {  # boot <label> <readyz-port> -- <cmd...>
  local label="$1" port="$2"; shift 3
  "$@" > "$WORK/$label.log" 2>&1 &
  PIDS+=($!)
  for _ in $(seq 1 30); do curl -sf "http://localhost:$port/readyz" >/dev/null 2>&1 && break; sleep 1; done
  if curl -sf "http://localhost:$port/readyz" >/dev/null 2>&1; then
    pass "$label healthy"
  else
    fail "$label unhealthy"; tail -20 "$WORK/$label.log"; exit 1
  fi
}

boot control $ZEROSHIP_CONTROL_PORT -- "$BIN/zeroship-control" --port $ZEROSHIP_CONTROL_PORT \
  --blob-store "$WORK/blobs"
boot migrated $ZEROSHIP_MIGRATE_SERVER_PORT -- "$BIN/zeroship-migrate-server" --port $ZEROSHIP_MIGRATE_SERVER_PORT \
  --tmp-dir "$WORK/migrated-tmp"
# Reproduce the production Caddy path split without adding Caddy as a harness
# dependency: /v1 belongs to migrate-server and every other path to control.
# The project keeps one shared URL, so both migrate and deploy exercise the
# same creator contract as the tracked edge.
boot edge $ZEROSHIP_EDGE_PORT -- node -e '
const http = require("http");
const [edgePort, controlPort, migratePort] = process.argv.slice(1).map(Number);
http.createServer((request, response) => {
  const port = request.url.startsWith("/v1/") ? migratePort : controlPort;
  const upstream = http.request({
    hostname: "127.0.0.1",
    port,
    path: request.url,
    method: request.method,
    headers: request.headers,
  }, incoming => {
    response.writeHead(incoming.statusCode, incoming.headers);
    incoming.pipe(response);
  });
  upstream.on("error", () => {
    response.writeHead(502, {"content-type": "application/json"});
    response.end("{\"error\":\"edge upstream unavailable\"}");
  });
  request.pipe(upstream);
}).listen(edgePort, "127.0.0.1");
' "$ZEROSHIP_EDGE_PORT" "$ZEROSHIP_CONTROL_PORT" "$ZEROSHIP_MIGRATE_SERVER_PORT"
boot worker $ZEROSHIP_WORKER_PORT -- "$BIN/zeroship-worker" --port $ZEROSHIP_WORKER_PORT --threads 2 \
  --control-url "http://localhost:$ZEROSHIP_CONTROL_PORT" --blob-store "$WORK/blobs" --poll-interval 2
boot gate $ZEROSHIP_GATEWAY_PORT -- "$BIN/zeroship-gate" --port $ZEROSHIP_GATEWAY_PORT \
  --control-url "http://localhost:$ZEROSHIP_CONTROL_PORT" \
  --worker-urls "http://localhost:$ZEROSHIP_WORKER_PORT" --blob-store "$WORK/blobs" \
  --blob-cache-disk-root "$WORK/blob-cache" --poll-interval 2 \
  --broker-secret-file "$WORK/gate-secret"

# ---------------------------------------------------------------------------
echo ""
echo "=== mint an admin platform bearer and create its app ==="
# The scope string is the deleted permission_tokens policy's action list,
# one-for-one: it becomes the token policy control intersects with the
# owner's own authority.
SCOPE="apps:read apps:write apps:deploy apps:archive deployments:read deployments:rollback env:read env:write secrets:read secrets:write"
OWNER="$(node -e 'console.log(require("crypto").randomUUID())')"
docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.users (id, email, name, email_verified_at)
VALUES ('$OWNER', 'projcfg-$OWNER@zeroship.test'::citext, 'ProjCfg E2E', NOW());
SQL
ADMIN_TOKEN="$(e2e_mint_platform_bearer "$OWNER" "$SCOPE")"
[ "$(echo -n "$ADMIN_TOKEN" | awk -F. '{print NF}')" = "3" ] && pass "minted platform bearer" || { fail "bearer mint failed"; exit 1; }

APP_JSON="$(curl -s -X POST "http://localhost:$ZEROSHIP_CONTROL_PORT/api/apps" \
  -H 'Content-Type: application/json' -H "Authorization: Bearer $ADMIN_TOKEN" -d '{"name":"projcfg-e2e"}')"
APP_ID="$(echo "$APP_JSON" | jget '.id')"
[ -n "$APP_ID" ] && pass "created app projcfg-e2e ($APP_ID)" || { fail "create app: $APP_JSON"; exit 1; }
docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.app_members (app_id, user_id, role) VALUES ('$APP_ID', '$OWNER', 'owner')
ON CONFLICT (app_id, user_id) DO UPDATE SET role = 'owner';
SQL

# ---------------------------------------------------------------------------
echo ""
echo "=== prepare the creator project from zeroship.jsonc ==="
# A COPY of examples/db-todos, so the repo's own tree is untouched and the
# `app` id can be written into the file the way a creator would.
#
# `migrations.out` is DELIBERATELY NOT the default: `generated/elsewhere`.
# With the deleted `DEFAULT_IR_PATH` this project was unreachable by
# `zeroship migrate` without typing the path, which is the defect this harness
# guards against.
APP_DIR="$WORK/project"
mkdir -p "$APP_DIR"
cp -a "$SRC_APP/dist" "$APP_DIR/dist"
cp -a "$SRC_APP/migrations" "$APP_DIR/migrations"
mkdir -p "$APP_DIR/generated/elsewhere"
cp -a "$SRC_APP/generated/zeroship/." "$APP_DIR/generated/elsewhere/"

cat > "$APP_DIR/zeroship.jsonc" <<JSONC
// The whole point of this harness: the deploy target lives here, not in the
// shell history. Note migrations.out is NOT the schema default.
{
  "\$schema": "https://zeroship.ai/schema/project-v1.json",
  "name": "projcfg-e2e",
  "app": "$APP_ID",
  "control": "http://localhost:$ZEROSHIP_EDGE_PORT",
  "runtime_date": "2026-08-14",
  "build": { "mode": "full", "dist": "dist", "output": "dist/app.zship" },
  "migrations": { "dir": "migrations", "out": "generated/elsewhere" },
  "secrets": []
}
JSONC
pass "wrote $APP_DIR/zeroship.jsonc (migrations.out = generated/elsewhere)"

# The token is a CREDENTIAL, not a deploy target, so it comes from the
# environment. Nothing below names an app, a control plane or a path.
export ZEROSHIP_TOKEN="$ADMIN_TOKEN"

echo ""
echo "--- transcript: zeroship config show ---"
( cd "$APP_DIR" && "$BIN/zeroship" config show ) | tee "$WORK/config-show.txt"
grep -q "\"out\":\"generated/elsewhere\"" "$WORK/config-show.txt" \
  && pass "config show resolves migrations.out from the file" \
  || fail "config show did not resolve migrations.out: $(cat "$WORK/config-show.txt")"

# Database creation is explicit and uses the same shared edge origin as the
# config-driven migrate below. The edge's /v1/* split must carry both route
# shapes to migrate-server.
CREATE_CODE="$(curl -sS -o "$WORK/create-database-response.json" -w '%{http_code}' -X POST \
  "http://localhost:$ZEROSHIP_EDGE_PORT/v1/databases/$APP_ID" \
  -H "Authorization: Bearer $ADMIN_TOKEN")"
if [[ "$CREATE_CODE" != 2?? ]]; then
  fail "database create failed through the shared edge (http=$CREATE_CODE): $(cat "$WORK/create-database-response.json")"
  tail -20 "$WORK/migrated.log"
  exit 1
fi

echo ""
echo "--- transcript: zeroship migrate (no flags) ---"
# The local edge above sends the CLI's current /v1/apps/* route directly to
# migrate-server. A narrower /v1/databases/* matcher would send this request to
# control and reproduce the 404 that held the direct-edge branch.
MIG_OUT="$( cd "$APP_DIR" && "$BIN/zeroship" migrate 2>&1 )"; MIG_RC=$?
echo "$MIG_OUT"
if [ "$MIG_RC" = 0 ] && grep -qE 'Applied [1-9][0-9]* migration op' <<<"$MIG_OUT"; then
  pass "zeroship migrate with NO --app/--control/path applied the schema"
else
  fail "zeroship migrate failed (rc=$MIG_RC)"; tail -20 "$WORK/migrated.log"
fi
grep -q "app = $APP_ID (from zeroship.jsonc)" <<<"$MIG_OUT" \
  && pass "migrate printed the app's provenance before the POST" \
  || fail "migrate did not print app provenance"
grep -q "control = http://localhost:$ZEROSHIP_EDGE_PORT (from zeroship.jsonc)" <<<"$MIG_OUT" \
  && pass "migrate printed the control plane's provenance before the POST" \
  || fail "migrate did not print control provenance"
grep -qF "migrations = $APP_DIR/generated/elsewhere/migrations.ir.json" <<<"$MIG_OUT" \
  && pass "migrate resolved the IR path from migrations.out, not from a constant" \
  || fail "migrate did not name the resolved IR path"

echo ""
echo "--- transcript: zeroship deploy (no flags, no positional) ---"
DEP_OUT="$( cd "$APP_DIR" && "$BIN/zeroship" deploy 2>&1 )"; DEP_RC=$?
echo "$DEP_OUT"
if [ "$DEP_RC" = 0 ] && grep -q "deploy_hash" <<<"$DEP_OUT"; then
  pass "zeroship deploy with NO arguments at all uploaded build.output"
else
  fail "zeroship deploy failed (rc=$DEP_RC)"; tail -20 "$WORK/control.log"
fi
grep -q "app = $APP_ID (from zeroship.jsonc)" <<<"$DEP_OUT" \
  && pass "deploy printed the app's provenance before the upload" \
  || fail "deploy did not print app provenance"
sleep 5   # route + version sync to gateway + worker

# ---------------------------------------------------------------------------
echo ""
echo "=== verify database-backed RPC reads and writes ==="
zs_frame() {
  node -e '
const fs = require("fs");
const [out, method, url, body] = process.argv.slice(1);
const meta = Buffer.from(JSON.stringify({ method, url, headers: [["content-type","application/json"]] }), "utf8");
const len = Buffer.alloc(4); len.writeUInt32LE(meta.length, 0);
fs.writeFileSync(out, Buffer.concat([len, meta, Buffer.from(body, "utf8")]));
' "$@"
}
dispatch() {  # dispatch <frame-file> -> body on stdout
  curl -s -X POST "http://localhost:$ZEROSHIP_WORKER_PORT/dispatch/$APP_ID" \
    -H "Authorization: Bearer $ZEROSHIP_WORKER_KEY" \
    -H 'content-type: application/octet-stream' --data-binary @"$1"
}

zs_frame "$WORK/f-users.bin" POST "http://projcfg-e2e.localhost/__zeroship/v1/users.public" '{"json":{}}'
U_BODY="$(dispatch "$WORK/f-users.bin")"
echo "  users.public -> $(head -c 200 <<<"$U_BODY")"
UID_VAL="$(echo "$U_BODY" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const o=JSON.parse(s);console.log((o.json&&o.json.id)||"")}catch(e){console.log("")}})')"
if [ -n "$UID_VAL" ]; then
  pass "env.db read returned a row (so zeroship migrate created the per-app role)"
else
  fail "users.public returned no row: $U_BODY"; tail -20 "$WORK/worker.log"
fi

if [ -n "$UID_VAL" ]; then
  zs_frame "$WORK/f-create.bin" POST "http://x/__zeroship/v1/todos.create" \
    "$(node -e 'process.stdout.write(JSON.stringify({json:{userId:process.argv[1],title:"projcfg e2e todo"}}))' "$UID_VAL")"
  C_BODY="$(dispatch "$WORK/f-create.bin")"
  grep -q '"json"' <<<"$C_BODY" && pass "env.db mutation todos.create succeeded" || fail "todos.create: $C_BODY"

  zs_frame "$WORK/f-list.bin" POST "http://x/__zeroship/v1/todos.list" \
    "$(node -e 'process.stdout.write(JSON.stringify({json:{userId:process.argv[1]}}))' "$UID_VAL")"
  L_BODY="$(dispatch "$WORK/f-list.bin")"
  echo "  todos.list -> $(head -c 200 <<<"$L_BODY")"
  grep -q 'projcfg e2e todo' <<<"$L_BODY" \
    && pass "env.db query returned the row that was just written" \
    || fail "todos.list missing the row: $L_BODY"
fi

# ---------------------------------------------------------------------------
echo ""
echo "=== verify the hardcoded migration path stays removed ==="
# ONE VARIABLE. Same project, same working directory, same command; the only
# difference is where the migration set is looked up.
#
#   A) the config-driven path -> the file exists and was applied above
#   B) the deleted DEFAULT_IR_PATH literal -> the file does NOT exist
#
# B is what `zeroship migrate` did before this change whenever no positional
# path was typed. The build wrote `generated/elsewhere/`; the constant said
# `generated/zeroship/`; nothing reconciled them.
OLD_CONST="generated/zeroship/migrations.ir.json"
NEW_PATH="generated/elsewhere/migrations.ir.json"
if [ -f "$APP_DIR/$NEW_PATH" ]; then
  pass "A: the path the config resolves ($NEW_PATH) EXISTS in the project"
else
  fail "A: $NEW_PATH is missing - the fixture is wrong, not the CLI"
fi
if [ ! -e "$APP_DIR/$OLD_CONST" ]; then
  pass "B: the deleted constant's path ($OLD_CONST) does NOT exist in this project"
else
  fail "B: $OLD_CONST exists - the fixture does not reproduce the split"
fi
OLD_OUT="$( cd "$APP_DIR" && "$BIN/zeroship" migrate "$OLD_CONST" 2>&1 )"; OLD_RC=$?
echo "  old-constant path -> rc=$OLD_RC: $(head -c 160 <<<"$OLD_OUT")"
if [ "$OLD_RC" != 0 ] && grep -q "failed to read" <<<"$OLD_OUT"; then
  pass "B: driving the removed hardcoded path FAILS on this project"
else
  fail "B: the old path did not fail (rc=$OLD_RC) - the regression probe is invalid"
fi

# ...and with NO config file at all, the CLI REFUSES rather than guessing that
# same constant. This is the arm that makes the deletion permanent: a
# reintroduced fallback would make this succeed.
NOCFG="$WORK/nocfg"; mkdir -p "$NOCFG/generated/zeroship"
cp "$APP_DIR/$NEW_PATH" "$NOCFG/$OLD_CONST"
NOCFG_OUT="$( cd "$NOCFG" && "$BIN/zeroship" migrate --app="$APP_ID" --control="http://localhost:$ZEROSHIP_CONTROL_PORT" 2>&1 )"; NOCFG_RC=$?
echo "  no-config, no-positional -> rc=$NOCFG_RC: $(head -c 200 <<<"$NOCFG_OUT")"
if [ "$NOCFG_RC" != 0 ] && grep -q "no migration set to apply" <<<"$NOCFG_OUT"; then
  pass "with no zeroship.jsonc and no path, migrate REFUSES instead of guessing generated/zeroship"
else
  fail "migrate produced a path with nothing to derive it from (rc=$NOCFG_RC) - the constant is back"
fi

echo ""
echo "============================================"
echo "  Results: $PASS passed, $FAIL failed"
echo "============================================"
[ "$FAIL" -eq 0 ] || exit 1
