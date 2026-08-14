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
# THE PRE-FIX HALF MATTERS AS MUCH AS THE GREEN HALF (stage 6). The defect this
# file removes is four independent derivations of two paths
# (docs/proposals/2026-08-14-project-config.md 2.1), and the sharpest of them
# was a Rust constant, `DEFAULT_IR_PATH = "generated/zeroship/migrations.ir.json"`,
# that the CLI used when no positional path was given. A project whose build
# wrote anywhere else got a `failed to read` from a path it never chose. Stage 6
# builds exactly that project - `migrations.out` pointed somewhere else - and
# shows the deleted constant's literal failing on it while the config-driven
# resolution succeeds. Same project, same command, one variable.
#
# WHAT THIS DOES NOT PROVE:
#   - It does not exercise `--env=`; the environments overlay is covered by the
#     Rust unit tests and by `tests/project_config_gate.sh`, not here.
#   - It does not prove the writeback, because the auto-create path needs an
#     app that does NOT exist and this harness creates one up front so it can
#     grant ownership (migrated requires a literal `role = 'owner'` row).
#   - The RPC is driven at the worker's `/dispatch`, not through the gateway.
#     db-todos' procedures declare no `auth`, so the gateway fail-closes them at
#     401 without an OIDC session - the same documented gate
#     `tests/e2e_app_primitives.sh` records. This harness is about the CONFIG
#     path, and routing the RPC through a login it does not need would only add
#     ways for it to fail for unrelated reasons.
#
# Requires: docker, openssl, node, and a release build including
#   cargo build --release
#   cargo build --release -p zeroship-migrate-adapter --features platform-cli \
#         --bin zeroship-platform-migrate
# plus a built examples/db-todos (`pnpm --filter db-todos build` or `pnpm build`).
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release"
JOSE_JS="$ROOT/node_modules/.pnpm/jose@6.2.3/node_modules/jose/dist/webapi/index.js"

# Own port band + container name so this can run beside the other harnesses.
: "${ZEROSHIP_CONTROL_PORT:=9131}"
: "${ZEROSHIP_WORKER_PORT:=8091}"
: "${ZEROSHIP_GATEWAY_PORT:=8021}"
: "${ZEROSHIP_MIGRATED_PORT:=9231}"
: "${PG_PORT:=5461}"
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

for b in zeroship zeroship-control zeroship-gate zeroship-worker zeroship-platform-migrate zeroship-migrated; do
  [ -x "$BIN/$b" ] || { echo "missing $BIN/$b - run cargo build --release, then cargo build --release -p zeroship-migrate-adapter --features platform-cli --bin zeroship-platform-migrate"; exit 2; }
done
SRC_APP="$ROOT/examples/db-todos"
[ -f "$SRC_APP/dist/app.zship" ] || { echo "missing $SRC_APP/dist/app.zship - run: (cd $SRC_APP && pnpm build)"; exit 2; }
[ -f "$JOSE_JS" ] || { echo "missing jose at $JOSE_JS"; exit 2; }
command -v docker >/dev/null || { echo "docker required"; exit 2; }
command -v openssl >/dev/null || { echo "openssl required"; exit 2; }

WORK="$(mktemp -d -t zs-e2e-projcfg-XXXXXX)"
mkdir -p "$WORK/blobs" "$WORK/blob-cache"

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 1: ephemeral Postgres + platform migrations ==="
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
if "$BIN/zeroship-platform-migrate" --database-url "$DBURL" \
    --migrations-dir "$ROOT/db/migrations-ts" \
    --project-schema zeroship --project-id zeroship > "$WORK/platmig.log" 2>&1; then
  pass "platform migrations applied from scratch"
else
  fail "zeroship-platform-migrate FAILED"; tail -20 "$WORK/platmig.log"; exit 1
fi

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 2: control + migrated + worker + gateway ==="
openssl genpkey -algorithm ed25519 -out "$WORK/signing-key.pem" 2>/dev/null
chmod 600 "$WORK/signing-key.pem"
openssl rand -base64 48 > "$WORK/gate-secret"; chmod 600 "$WORK/gate-secret"
for p in $ZEROSHIP_CONTROL_PORT $ZEROSHIP_WORKER_PORT $ZEROSHIP_GATEWAY_PORT $ZEROSHIP_MIGRATED_PORT; do
  lsof -ti :$p 2>/dev/null | xargs -r kill -9 2>/dev/null || true
done

ZEROSHIP_CONTROL_SIGNING_KEY_FILE="$WORK/signing-key.pem"
ZEROSHIP_GATEWAY_SIGNING_KEY_FILE="$ZEROSHIP_CONTROL_SIGNING_KEY_FILE"
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
  --blob-store "$WORK/blobs" --signing-key-file "$WORK/signing-key.pem" \
  --migrated-url "http://localhost:$ZEROSHIP_MIGRATED_PORT"
boot migrated $ZEROSHIP_MIGRATED_PORT -- "$BIN/zeroship-migrated" --port $ZEROSHIP_MIGRATED_PORT \
  --signing-key-file "$WORK/signing-key.pem" --tmp-dir "$WORK/migrated-tmp"
boot worker $ZEROSHIP_WORKER_PORT -- "$BIN/zeroship-worker" --port $ZEROSHIP_WORKER_PORT --threads 2 \
  --control-url "http://localhost:$ZEROSHIP_CONTROL_PORT" --blob-store "$WORK/blobs" --poll-interval 2
boot gate $ZEROSHIP_GATEWAY_PORT -- "$BIN/zeroship-gate" --port $ZEROSHIP_GATEWAY_PORT \
  --control-url "http://localhost:$ZEROSHIP_CONTROL_PORT" \
  --worker-urls "http://localhost:$ZEROSHIP_WORKER_PORT" --blob-store "$WORK/blobs" \
  --blob-cache-disk-root "$WORK/blob-cache" --poll-interval 2 \
  --broker-secret-file "$WORK/gate-secret"

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 3: admin PAT + an app to own ==="
POLICY_JSON='{"name":"e2e-admin","statements":[{"effect":"allow","actions":["apps:read","apps:write","apps:deploy","apps:delete","deployments:read","deployments:rollback","env:read","env:write","secrets:read","secrets:write"],"resources":[{"type":"any"}],"conditions":[]}]}'
POLICY_HASH="$(node -e '
const {createHash}=require("crypto");
function c(v){if(v===null||typeof v==="number"||typeof v==="boolean")return JSON.stringify(v);
if(typeof v==="string")return JSON.stringify(v);
if(Array.isArray(v))return "["+v.map(c).join(",")+"]";
return "{"+Object.keys(v).sort().map(k=>JSON.stringify(k)+":"+c(v[k])).join(",")+"}";}
process.stdout.write(createHash("sha256").update(c(JSON.parse(process.argv[1]))).digest("hex"));
' "$POLICY_JSON")"
OWNER="$(node -e 'console.log(require("crypto").randomUUID())')"
TOKID="$(node -e 'console.log(require("crypto").randomUUID())')"
EXP=$(( $(date +%s) + 86400 ))
docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.users (id, email, name, email_verified_at)
VALUES ('$OWNER', 'projcfg-$OWNER@zeroship.test'::citext, 'ProjCfg E2E', NOW());
INSERT INTO zeroship.platform_admin_roles (user_id, role, granted_by)
VALUES ('$OWNER', 'admin', '$OWNER');
INSERT INTO zeroship.permission_tokens (id, owner_id, kind, name, policies, policy_hash, expires_at)
VALUES ('$TOKID', '$OWNER', 'pat', 'projcfg harness', '$POLICY_JSON'::jsonb, '$POLICY_HASH', to_timestamp($EXP));
SQL
PAT="$(node --input-type=module -e '
import { readFileSync } from "node:fs";
import { createHash, randomBytes } from "node:crypto";
import { importPKCS8, exportJWK, SignJWT } from "file://'"$JOSE_JS"'";
const [pem, owner, tid, phash, exp] = process.argv.slice(1);
const key = await importPKCS8(readFileSync(pem,"utf8"), "EdDSA", { extractable:true });
const x = (await exportJWK(key)).x;
const kid = createHash("sha256").update(`{"crv":"Ed25519","kty":"OKP","x":"${x}"}`).digest("base64url");
const jwt = await new SignJWT({ sub:owner, owner, tid, jti:tid, scope:"pat", policy_hash:phash, nonce:randomBytes(32).toString("base64url") })
  .setProtectedHeader({ alg:"EdDSA", typ:"pat+jwt", kid })
  .setIssuer("https://api.zeroship.ai").setAudience("control.zeroship.ai")
  .setIssuedAt(Math.floor(Date.now()/1000)).setExpirationTime(Number(exp))
  .sign(key);
process.stdout.write(jwt);
' "$WORK/signing-key.pem" "$OWNER" "$TOKID" "$POLICY_HASH" "$EXP")"
[ "$(echo -n "$PAT" | awk -F. '{print NF}')" = "3" ] && pass "minted pat+jwt" || { fail "PAT mint failed"; exit 1; }

APP_JSON="$(curl -s -X POST "http://localhost:$ZEROSHIP_CONTROL_PORT/api/apps" \
  -H 'Content-Type: application/json' -H "Authorization: Bearer $PAT" -d '{"name":"projcfg-e2e"}')"
APP_ID="$(echo "$APP_JSON" | jget '.id')"
[ -n "$APP_ID" ] && pass "created app projcfg-e2e ($APP_ID)" || { fail "create app: $APP_JSON"; exit 1; }
docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.app_members (app_id, user_id, role) VALUES ('$APP_ID', '$OWNER', 'owner')
ON CONFLICT (app_id, user_id) DO UPDATE SET role = 'owner';
SQL

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 4: the creator project, with a zeroship.jsonc and nothing else ==="
# A COPY of examples/db-todos, so the repo's own tree is untouched and the
# `app` id can be written into the file the way a creator would.
#
# `migrations.out` is DELIBERATELY NOT the default: `generated/elsewhere`.
# With the deleted `DEFAULT_IR_PATH` this project was unreachable by
# `zeroship migrate` without typing the path, which is the defect (stage 6).
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
  "control": "http://localhost:$ZEROSHIP_CONTROL_PORT",
  "runtime_date": "2026-08-14",
  "build": { "mode": "full", "dist": "dist", "output": "dist/app.zship" },
  "migrations": { "dir": "migrations", "out": "generated/elsewhere" },
  "secrets": []
}
JSONC
pass "wrote $APP_DIR/zeroship.jsonc (migrations.out = generated/elsewhere)"

# The token is a CREDENTIAL, not a deploy target, so it comes from the
# environment. Nothing below names an app, a control plane or a path.
export ZEROSHIP_TOKEN="$PAT"

echo ""
echo "--- transcript: zeroship config show ---"
( cd "$APP_DIR" && "$BIN/zeroship" config show ) | tee "$WORK/config-show.txt"
grep -q "\"out\":\"generated/elsewhere\"" "$WORK/config-show.txt" \
  && pass "config show resolves migrations.out from the file" \
  || fail "config show did not resolve migrations.out: $(cat "$WORK/config-show.txt")"

echo ""
echo "--- transcript: zeroship migrate (no flags) ---"
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
grep -q "control = http://localhost:$ZEROSHIP_CONTROL_PORT (from zeroship.jsonc)" <<<"$MIG_OUT" \
  && pass "migrate printed the control plane's provenance before the POST" \
  || fail "migrate did not print control provenance"
grep -q "migrations = generated/elsewhere/migrations.ir.json" <<<"$MIG_OUT" \
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
echo "=== Stage 5: a database-backed RPC returns real rows ==="
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
echo "=== Stage 6: the pre-fix failure, reproduced on the same project ==="
# ONE VARIABLE. Same project, same working directory, same command; the only
# difference is where the migration set is looked up.
#
#   A) the config-driven path            -> the file exists   (stage 4 proved it applies)
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
  pass "B: driving the old hardcoded path FAILS on this project (the pre-fix behaviour)"
else
  fail "B: the old path did not fail (rc=$OLD_RC) - stage 6 is not measuring the defect"
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
