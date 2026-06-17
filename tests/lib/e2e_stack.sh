# shellcheck shell=bash
# ============================================================================
# tests/lib/e2e_stack.sh — shared E2E bring-up for the zeroship platform.
#
# Single source of truth for the full local stack the gateway-level E2E
# harnesses need:
#
#   stack_up        ephemeral Postgres (docker) + the FULL platform migration
#                   set from db/migrations (V0001→latest) applied from scratch
#                   via the `zeroship-migrate` bin (Platform profile), then
#                   control + worker + gateway booted with --dev-insecure and
#                   health-polled. Non-blocking: binaries run in the background;
#                   the function returns once all three are health-green.
#   mint_admin_pat  OFFLINE-mints a platform-admin PAT (inserts users +
#                   platform_admin_roles + permission_tokens rows, signs an
#                   EdDSA pat+jwt with the workspace `jose`). Exports $PAT.
#   deploy_zship    create an app named <slug> via the control API and deploy a
#                   prebuilt .zship with `zeroship deploy --token=$PAT`; echoes
#                   the created app id on stdout (return 0), or returns 1.
#   stack_down      kill the binary PIDs, docker rm -f the PG container, rm $WORK.
#
# This file is SOURCED, not executed. It assumes `set -uo pipefail` in the
# caller and that the caller defines pass()/fail() helpers IF it wants the
# bring-up to emit ✓/✗ lines (stack_up calls them when present; otherwise it
# falls back to plain echo). Errors during bring-up return non-zero so the
# caller can decide whether to abort.
#
# Tunables (export BEFORE calling stack_up; sensible defaults pick a private
# port band so multiple harnesses can run back-to-back without colliding):
#   CONTROL_PORT WORKER_PORT GATE_PORT PG_PORT   — listen ports
#   PG_CONTAINER                                 — docker container name
#   WORKER_THREADS                               — worker --worker-threads
#   E2E_ROOT                                     — repo root (auto-derived)
#
# Exports after stack_up: CONTROL_PORT WORKER_PORT GATE_PORT PG_CONTAINER WORK
#   PIDFILE (newline-separated binary PIDs under $WORK), DBURL.
# ============================================================================

# --- repo root + binary dir (derive once) ----------------------------------
if [ -z "${E2E_ROOT:-}" ]; then
  # this file lives at <root>/tests/lib/e2e_stack.sh
  E2E_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fi
E2E_BIN="$E2E_ROOT/target/release"
E2E_JOSE_JS="$E2E_ROOT/node_modules/.pnpm/jose@6.2.3/node_modules/jose/dist/webapi/index.js"

# --- defaults (private/non-colliding band; override before stack_up) --------
: "${CONTROL_PORT:=9120}"
: "${WORKER_PORT:=8098}"
: "${GATE_PORT:=8012}"
: "${PG_PORT:=5454}"
: "${PG_CONTAINER:=zs-e2e-stack-pg}"
: "${WORKER_THREADS:=2}"

# --- emit helpers: prefer caller-provided pass/fail, else plain echo --------
_stk_ok()   { if declare -F pass >/dev/null 2>&1; then pass "$1"; else echo "  ✓ $1"; fi; }
_stk_bad()  { if declare -F fail >/dev/null 2>&1; then fail "$1"; else echo "  ✗ $1"; fi; }

# node helper: read a JSON field from stdin (e.g. `... | _stk_jget '.id'`)
_stk_jget() { node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{try{const o=JSON.parse(s);console.log(o$1??'')}catch(e){console.log('')}})"; }

# --- preflight: required binaries + tooling ---------------------------------
stack_preflight() {
  local b
  for b in zeroship zeroship-control zeroship-gate zeroship-worker zeroship-migrate; do
    [ -x "$E2E_BIN/$b" ] || { _stk_bad "missing $E2E_BIN/$b — run: cargo build --release"; return 2; }
  done
  [ -f "$E2E_JOSE_JS" ] || { _stk_bad "missing jose at $E2E_JOSE_JS"; return 2; }
  command -v docker  >/dev/null || { _stk_bad "docker required"; return 2; }
  command -v openssl >/dev/null || { _stk_bad "openssl required"; return 2; }
  command -v node    >/dev/null || { _stk_bad "node required"; return 2; }
  return 0
}

# --- stack_up: PG + migrations + control/worker/gateway, non-blocking -------
stack_up() {
  stack_preflight || return $?

  WORK="$(mktemp -d -t zs-e2e-stack-XXXXXX)"
  mkdir -p "$WORK/blobs" "$WORK/blob-cache"
  PIDFILE="$WORK/pids"
  : > "$PIDFILE"
  DBURL="postgres://postgres:zeroship@localhost:$PG_PORT/zeroship"
  export WORK PIDFILE DBURL CONTROL_PORT WORKER_PORT GATE_PORT PG_CONTAINER

  # --- ephemeral Postgres ---------------------------------------------------
  docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
  docker run --name "$PG_CONTAINER" -d -p "$PG_PORT:5432" \
    -e POSTGRES_PASSWORD=zeroship -e POSTGRES_USER=postgres -e POSTGRES_DB=zeroship \
    postgres:16 -c max_connections=300 >/dev/null || { _stk_bad "docker run postgres failed"; return 1; }
  local i
  for i in $(seq 1 30); do docker exec "$PG_CONTAINER" pg_isready -U postgres >/dev/null 2>&1 && break; sleep 1; done
  if docker exec "$PG_CONTAINER" pg_isready -U postgres >/dev/null 2>&1; then
    _stk_ok "ephemeral PG ready on :$PG_PORT"
  else
    _stk_bad "PG never became ready"; return 1
  fi

  if [ -f "$E2E_ROOT/ops/postgres-init.sql" ]; then
    docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 < "$E2E_ROOT/ops/postgres-init.sql" >/dev/null 2>&1 \
      && _stk_ok "applied ops/postgres-init.sql" || _stk_bad "postgres-init.sql failed"
  fi

  local mig_log="$WORK/migrate.log"
  if "$E2E_BIN/zeroship-migrate" migrate \
      --dir "$E2E_ROOT/db/migrations" \
      --database-url "postgres://postgres:zeroship@localhost:$PG_PORT/zeroship" \
      --profile platform --yes > "$mig_log" 2>&1; then
    _stk_ok "platform migrations applied cleanly from scratch (zeroship-migrate)"
  else
    _stk_bad "zeroship-migrate FAILED (see $mig_log)"; tail -20 "$mig_log"; return 1
  fi

  # --- signing key + free the ports ----------------------------------------
  openssl genpkey -algorithm ed25519 -out "$WORK/signing-key.pem" 2>/dev/null
  chmod 600 "$WORK/signing-key.pem"
  local p
  for p in $CONTROL_PORT $WORKER_PORT $GATE_PORT; do lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done

  # --- control --------------------------------------------------------------
  "$E2E_BIN/zeroship-control" --port "$CONTROL_PORT" --db "$DBURL" \
    --blob-store "$WORK/blobs" --signing-key-file "$WORK/signing-key.pem" \
    --dev-insecure > "$WORK/control.log" 2>&1 &
  echo $! >> "$PIDFILE"
  for i in $(seq 1 30); do curl -sf "http://localhost:$CONTROL_PORT/health" >/dev/null 2>&1 && break; sleep 1; done
  curl -sf "http://localhost:$CONTROL_PORT/health" >/dev/null 2>&1 \
    && _stk_ok "control healthy" || { _stk_bad "control unhealthy"; tail -20 "$WORK/control.log"; return 1; }

  # --- worker ---------------------------------------------------------------
  "$E2E_BIN/zeroship-worker" --port "$WORKER_PORT" --worker-threads "$WORKER_THREADS" \
    --control "http://localhost:$CONTROL_PORT" --db "$DBURL" \
    --blob-store "$WORK/blobs" --poll-interval 2 --dev-insecure > "$WORK/worker.log" 2>&1 &
  echo $! >> "$PIDFILE"
  for i in $(seq 1 30); do curl -sf "http://localhost:$WORKER_PORT/health" >/dev/null 2>&1 && break; sleep 1; done
  curl -sf "http://localhost:$WORKER_PORT/health" >/dev/null 2>&1 \
    && _stk_ok "worker healthy" || { _stk_bad "worker unhealthy"; tail -20 "$WORK/worker.log"; return 1; }

  # --- gateway --------------------------------------------------------------
  "$E2E_BIN/zeroship-gate" --port "$GATE_PORT" --control "http://localhost:$CONTROL_PORT" \
    --workers "http://localhost:$WORKER_PORT" --blob-store "$WORK/blobs" \
    --blob-cache-disk-root "$WORK/blob-cache" --db "$DBURL" --poll-interval 2 \
    --signing-key-file "$WORK/signing-key.pem" \
    --dev-insecure > "$WORK/gate.log" 2>&1 &
  echo $! >> "$PIDFILE"
  for i in $(seq 1 30); do curl -sf "http://localhost:$GATE_PORT/health" >/dev/null 2>&1 && break; sleep 1; done
  curl -sf "http://localhost:$GATE_PORT/health" >/dev/null 2>&1 \
    && _stk_ok "gateway healthy" || { _stk_bad "gateway unhealthy"; tail -20 "$WORK/gate.log"; return 1; }

  return 0
}

# --- mint_admin_pat: offline-signed platform-admin PAT, exports $PAT --------
mint_admin_pat() {
  local policy_json policy_hash owner tokid exp
  policy_json='{"name":"e2e-admin","statements":[{"effect":"allow","actions":["apps:read","apps:write","apps:deploy","apps:delete","deployments:read","deployments:rollback","env:read","env:write","secrets:read","secrets:write"],"resources":[{"type":"any"}],"conditions":[]}]}'
  policy_hash="$(node -e '
const {createHash}=require("crypto");
function c(v){if(v===null||typeof v==="number"||typeof v==="boolean")return JSON.stringify(v);
if(typeof v==="string")return JSON.stringify(v);
if(Array.isArray(v))return "["+v.map(c).join(",")+"]";
return "{"+Object.keys(v).sort().map(k=>JSON.stringify(k)+":"+c(v[k])).join(",")+"}";}
process.stdout.write(createHash("sha256").update(c(JSON.parse(process.argv[1]))).digest("hex"));
' "$policy_json")"
  owner="$(node -e 'console.log(require("crypto").randomUUID())')"
  tokid="$(node -e 'console.log(require("crypto").randomUUID())')"
  exp=$(( $(date +%s) + 86400 ))
  docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.users (id, email, name, email_verified_at)
VALUES ('$owner', 'e2e-$owner@zeroship.test'::citext, 'E2E Stack Admin', NOW());
INSERT INTO zeroship.platform_admin_roles (user_id, role, granted_by)
VALUES ('$owner', 'admin', '$owner');
INSERT INTO zeroship.permission_tokens (id, owner_id, kind, name, policies, policy_hash, expires_at)
VALUES ('$tokid', '$owner', 'pat', 'e2e stack harness', '$policy_json'::jsonb, '$policy_hash', to_timestamp($exp));
SQL
  PAT="$(node --input-type=module -e '
import { readFileSync } from "node:fs";
import { createHash, randomBytes } from "node:crypto";
import { importPKCS8, exportJWK, SignJWT } from "file://'"$E2E_JOSE_JS"'";
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
' "$WORK/signing-key.pem" "$owner" "$tokid" "$policy_hash" "$exp")"
  export PAT
  if [ "$(echo -n "$PAT" | awk -F. '{print NF}')" = "3" ]; then
    _stk_ok "minted pat+jwt"
    return 0
  else
    _stk_bad "PAT mint failed: $PAT"
    return 1
  fi
}

# --- deploy_zship <slug> <path-to-.zship>: create app + deploy, echo app id -
deploy_zship() {
  local slug="$1" zship="$2"
  local j id dep
  j="$(curl -s -X POST "http://localhost:$CONTROL_PORT/api/apps" \
        -H 'Content-Type: application/json' -H "Authorization: Bearer $PAT" \
        -d "{\"name\":\"$slug\"}")"
  id="$(echo "$j" | _stk_jget '.id')"
  if [ -z "$id" ]; then echo "    create-app($slug) failed: $j" >&2; return 1; fi
  dep="$("$E2E_BIN/zeroship" deploy "$zship" --app="$id" --control="http://localhost:$CONTROL_PORT" --token="$PAT" 2>&1)"
  if ! echo "$dep" | grep -q "deploy_hash"; then echo "    deploy($slug) failed: $dep" >&2; return 1; fi
  echo "$id"
  return 0
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
