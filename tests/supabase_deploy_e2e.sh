#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# LIVE Supabase/GoTrue platform-auth deploy E2E.
#
# Proves the provider-native control auth chain against a real GoTrue:
#   GoTrue password session -> control device auth -> scripted approve ->
#   identity provision/link -> GoTrue refresh -> zeroship deploy with GoTrue
#   bearer -> gateway serves the deployed app.
#
# This uses a tiny localhost prefix proxy so control can talk to GoTrue with the
# hosted-Supabase URL shape (`{SUPABASE_URL}/auth/v1/...`) while the raw GoTrue
# container remains the actual auth server.
# ---------------------------------------------------------------------------
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release"
STARTER="$ROOT/examples/starter"
ZSHIP="$STARTER/dist/app.zship"

GOTRUE_IMAGE="${GOTRUE_IMAGE:-ghcr.io/supabase/auth:v2.178.0}"
RUN_ID="zs-supabase-e2e-$$-$RANDOM"
NETWORK="$RUN_ID-net"
CONTROL_PG_CONTAINER="$RUN_ID-control-pg"
GOTRUE_PG_CONTAINER="$RUN_ID-gotrue-pg"
GOTRUE_CONTAINER="$RUN_ID-gotrue"

SUPABASE_E2E_MODE="${SUPABASE_E2E_MODE:-hs256}"
SUPABASE_E2E_MODE="${SUPABASE_E2E_MODE,,}"
case "$SUPABASE_E2E_MODE" in
  hs256|jwks) ;;
  *) echo "SUPABASE_E2E_MODE must be hs256 or jwks, got: $SUPABASE_E2E_MODE" >&2; exit 1 ;;
esac

JWT_SECRET="${SUPABASE_E2E_JWT_SECRET:-zs-supabase-e2e-jwt-secret-at-least-32-bytes}"
SUPABASE_JWT_KID="${SUPABASE_E2E_JWT_KID:-zs-supabase-e2e-rs256}"
SUPABASE_JWT_PRIVATE_JWK=""
GOTRUE_JWT_KEYS=""
CONTROL_KEY="${CONTROL_KEY:-supabase-e2e-control-key}"
WORKER_KEY="${WORKER_KEY:-supabase-e2e-worker-key-0123456789abcdef}"
MASTER_KEY="${MASTER_KEY:-supabase-e2e-master-key-0123456789abcdef}"
APP_NAME="supabase-e2e-$(date +%s)-$RANDOM"

PASS=0
FAIL=0
PIDS=()
WORK=""

red() { printf '\033[31m%s\033[0m\n' "$1"; }
green() { printf '\033[32m%s\033[0m\n' "$1"; }
yellow() { printf '\033[33m%s\033[0m\n' "$1"; }
step() { yellow "=== $1 ==="; }
pass() { PASS=$((PASS + 1)); echo "  PASS $1"; }

dump_debug() {
  [ -n "${WORK:-}" ] || return 0
  echo "" >&2
  echo "Debug logs under $WORK" >&2
  for log in gotrue-proxy.log control.log worker.log gate.log; do
    if [ -f "$WORK/$log" ]; then
      echo "--- tail $log ---" >&2
      tail -40 "$WORK/$log" >&2 || true
    fi
  done
  if docker ps -a --format '{{.Names}}' | grep -qx "$GOTRUE_CONTAINER"; then
    echo "--- tail $GOTRUE_CONTAINER ---" >&2
    docker logs --tail 80 "$GOTRUE_CONTAINER" >&2 || true
  fi
}

fail() {
  FAIL=$((FAIL + 1))
  red "  FAIL $1" >&2
  dump_debug
  exit 1
}

cleanup() {
  local status=$?
  for pid in "${PIDS[@]:-}"; do
    kill "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
  docker rm -f "$GOTRUE_CONTAINER" "$GOTRUE_PG_CONTAINER" "$CONTROL_PG_CONTAINER" >/dev/null 2>&1 || true
  docker network rm "$NETWORK" >/dev/null 2>&1 || true
  [ -n "${WORK:-}" ] && rm -rf "$WORK"
  exit "$status"
}
trap cleanup EXIT

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

pick_port() {
  node -e '
const net = require("node:net");
const server = net.createServer();
server.listen(0, "127.0.0.1", () => {
  console.log(server.address().port);
  server.close();
});
'
}

CONTROL_PORT="${CONTROL_PORT:-$(pick_port)}"
WORKER_PORT="${WORKER_PORT:-$(pick_port)}"
GATE_PORT="${GATE_PORT:-$(pick_port)}"
CONTROL_PG_PORT="${CONTROL_PG_PORT:-$(pick_port)}"
GOTRUE_PORT="${GOTRUE_PORT:-$(pick_port)}"
GOTRUE_PROXY_PORT="${GOTRUE_PROXY_PORT:-$(pick_port)}"

CONTROL_URL="http://localhost:$CONTROL_PORT"
GATE_URL="http://localhost:$GATE_PORT"
SUPABASE_URL="http://localhost:$GOTRUE_PROXY_PORT"
GOTRUE_URL="$SUPABASE_URL/auth/v1"
SUPABASE_ISSUER="$SUPABASE_URL/auth/v1"
SUPABASE_JWKS_URL="$GOTRUE_URL/.well-known/jwks.json"
CONTROL_DB_URL="postgres://postgres:zeroship@localhost:$CONTROL_PG_PORT/zeroship"

json_get() {
  jq -r "$1 // empty"
}

json_string() {
  jq -Rs .
}

redact_jwt() {
  awk -F. '{ printf "%s...%s", substr($1, 1, 10), substr($3, length($3) - 7) }' <<<"$1"
}

decode_jwt_payload() {
  node -e '
const token = process.argv[1];
const part = token.split(".")[1] || "";
const padded = part + "=".repeat((4 - part.length % 4) % 4);
process.stdout.write(Buffer.from(padded.replace(/-/g, "+").replace(/_/g, "/"), "base64").toString("utf8"));
' "$1"
}

decode_jwt_header() {
  node -e '
const token = process.argv[1];
const part = token.split(".")[0] || "";
const padded = part + "=".repeat((4 - part.length % 4) % 4);
process.stdout.write(Buffer.from(padded.replace(/-/g, "+").replace(/_/g, "/"), "base64").toString("utf8"));
' "$1"
}

assert_jwt_mode_header() {
  local token="$1"
  local label="$2"
  [ "$SUPABASE_E2E_MODE" = "jwks" ] || return 0
  local header alg kid
  header="$(decode_jwt_header "$token")"
  alg="$(jq -r '.alg // empty' <<<"$header")"
  kid="$(jq -r '.kid // empty' <<<"$header")"
  echo "  $label header: alg=$alg kid=${kid:-<none>}"
  [ "$alg" = "RS256" ] || fail "$label was signed with $alg, expected RS256"
  [ "$kid" = "$SUPABASE_JWT_KID" ] || fail "$label kid was $kid, expected $SUPABASE_JWT_KID"
}

generate_jwks_signing_key() {
  SUPABASE_JWT_PRIVATE_JWK="$(node - "$SUPABASE_JWT_KID" <<'NODE'
const crypto = require("node:crypto");
const kid = process.argv[2];
const { privateKey } = crypto.generateKeyPairSync("rsa", {
  modulusLength: 2048,
  publicExponent: 0x10001,
});
const jwk = privateKey.export({ format: "jwk" });
jwk.alg = "RS256";
jwk.use = "sig";
jwk.key_ops = ["sign", "verify"];
jwk.kid = kid;
process.stdout.write(JSON.stringify(jwk));
NODE
)"
  GOTRUE_JWT_KEYS="[$SUPABASE_JWT_PRIVATE_JWK]"
  printf '%s\n' "$SUPABASE_JWT_PRIVATE_JWK" >"$WORK/supabase-rs256-private.jwk.json"
  printf '%s\n' "$GOTRUE_JWT_KEYS" >"$WORK/gotrue-jwt-keys.json"
  echo "  RS256 signing kid: $SUPABASE_JWT_KID"
  echo "  private JWK fields: $(jq -r 'keys | sort | join(",")' <<<"$SUPABASE_JWT_PRIVATE_JWK")"
}

mint_gotrue_key() {
  if [ "$SUPABASE_E2E_MODE" = "jwks" ]; then
    node - "$1" "$SUPABASE_ISSUER" <<'NODE'
const crypto = require("node:crypto");
const [role, issuer] = process.argv.slice(2);
const privateJwk = JSON.parse(process.env.SUPABASE_JWT_PRIVATE_JWK || "{}");
const key = crypto.createPrivateKey({ key: privateJwk, format: "jwk" });
const b64url = (value) => Buffer.from(value).toString("base64url");
const now = Math.floor(Date.now() / 1000);
const header = { alg: "RS256", typ: "JWT", kid: privateJwk.kid };
const payload = {
  iss: issuer,
  role,
  iat: now,
  exp: now + 10 * 365 * 24 * 60 * 60,
};
const signingInput = `${b64url(JSON.stringify(header))}.${b64url(JSON.stringify(payload))}`;
const sig = crypto.sign("RSA-SHA256", Buffer.from(signingInput), key).toString("base64url");
process.stdout.write(`${signingInput}.${sig}`);
NODE
    return 0
  fi

  node - "$1" "$JWT_SECRET" "$SUPABASE_ISSUER" <<'NODE'
const crypto = require("node:crypto");
const [role, secret, issuer] = process.argv.slice(2);
const b64url = (value) => Buffer.from(value).toString("base64url");
const now = Math.floor(Date.now() / 1000);
const header = { alg: "HS256", typ: "JWT" };
const payload = {
  iss: issuer,
  role,
  iat: now,
  exp: now + 10 * 365 * 24 * 60 * 60,
};
const signingInput = `${b64url(JSON.stringify(header))}.${b64url(JSON.stringify(payload))}`;
const sig = crypto.createHmac("sha256", secret).update(signingInput).digest("base64url");
process.stdout.write(`${signingInput}.${sig}`);
NODE
}

verify_gotrue_jwks() {
  [ "$SUPABASE_E2E_MODE" = "jwks" ] || return 0
  local jwks private_fields
  jwks="$(curl -fsS "$SUPABASE_JWKS_URL")" || fail "GoTrue JWKS endpoint was not reachable at $SUPABASE_JWKS_URL"
  printf '%s\n' "$jwks" >"$WORK/gotrue-jwks.json"
  jq -e --arg kid "$SUPABASE_JWT_KID" '
    (.keys | length) == 1
    and .keys[0].kid == $kid
    and .keys[0].kty == "RSA"
    and .keys[0].alg == "RS256"
  ' <<<"$jwks" >/dev/null || fail "GoTrue JWKS did not expose the expected RS256 public key: $jwks"
  private_fields="$(jq -r '.keys[0] | keys[] | select(. == "d" or . == "p" or . == "q" or . == "dp" or . == "dq" or . == "qi")' <<<"$jwks" | paste -sd, -)"
  [ -z "$private_fields" ] || fail "GoTrue JWKS exposed private RSA fields: $private_fields"
  echo "  GoTrue JWKS: $(jq -c '.keys[0] | {kid,kty,alg,use,key_ops,has_private:(has("d") or has("p") or has("q") or has("dp") or has("dq") or has("qi"))}' <<<"$jwks")"
}

post_json() {
  local url="$1"
  local body="$2"
  shift 2
  local out="$WORK/http-$(openssl rand -hex 4).json"
  local code
  code=$(curl -sS -o "$out" -w '%{http_code}' -X POST "$url" \
    -H 'Content-Type: application/json' "$@" -d "$body" || true)
  if [[ "$code" =~ ^2 ]]; then
    cat "$out"
  else
    fail "POST $url returned HTTP $code: $(cat "$out" 2>/dev/null || true)"
  fi
}

wait_http() {
  local url="$1"
  local label="$2"
  local i
  for i in $(seq 1 45); do
    if curl -sf "$url" >/dev/null 2>&1; then
      pass "$label"
      return 0
    fi
    sleep 1
  done
  fail "$label did not become ready at $url"
}

ensure_release_bins() {
  local missing=0
  local stale=0
  local build_stamp="$BIN/.supabase-e2e-build-stamp"
  local bin
  for bin in zeroship zeroship-control zeroship-worker zeroship-gate; do
    [ -x "$BIN/$bin" ] || missing=1
  done
  if [ "$missing" -eq 0 ]; then
    local source_roots=(
      "$ROOT/Cargo.toml" "$ROOT/Cargo.lock"
      "$ROOT/crates/control/Cargo.toml" "$ROOT/crates/control/src"
      "$ROOT/crates/worker/Cargo.toml" "$ROOT/crates/worker/src"
      "$ROOT/crates/gateway/Cargo.toml" "$ROOT/crates/gateway/src"
      "$ROOT/crates/cli/Cargo.toml" "$ROOT/crates/cli/src"
      "$ROOT/crates/core/Cargo.toml" "$ROOT/crates/core/src"
      "$ROOT/crates/bundle/Cargo.toml" "$ROOT/crates/bundle/src"
      "$ROOT/crates/authz/Cargo.toml" "$ROOT/crates/authz/src"
    )
    if [ ! -f "$build_stamp" ] || find "${source_roots[@]}" -type f -newer "$build_stamp" | grep -q .; then
      stale=1
    fi
  fi

  if [ "$missing" -eq 0 ] && [ "$stale" -eq 0 ]; then
    pass "release binaries present"
    return 0
  fi

  step "Build missing/stale release binaries"
  need_cmd nix
  nix develop --command cargo build --release \
    -p zeroship-control -p zeroship-worker -p zeroship-gateway -p zeroship \
    >"$WORK/cargo-build.log" 2>&1 || {
      tail -80 "$WORK/cargo-build.log" || true
      fail "release binary build failed"
    }
  touch "$build_stamp"
  pass "release binaries built"
}

ensure_starter_zship() {
  if [ -f "$ZSHIP" ]; then
    pass "starter .zship present ($(du -k "$ZSHIP" | cut -f1)KB)"
    return 0
  fi
  step "Build examples/starter"
  (cd "$STARTER" && pnpm build) >"$WORK/starter-build.log" 2>&1 || {
    tail -80 "$WORK/starter-build.log" || true
    fail "examples/starter build failed"
  }
  [ -f "$ZSHIP" ] || fail "examples/starter did not produce dist/app.zship"
  pass "starter .zship built ($(du -k "$ZSHIP" | cut -f1)KB)"
}

start_prefix_proxy() {
  cat >"$WORK/gotrue_prefix_proxy.mjs" <<'NODE'
import http from "node:http";

const listenPort = Number(process.argv[2]);
const target = new URL(process.argv[3]);
const prefix = "/auth/v1";
const hopByHop = new Set([
  "connection",
  "proxy-connection",
  "keep-alive",
  "transfer-encoding",
  "upgrade",
]);

function upstreamPath(rawUrl) {
  if (rawUrl === prefix) return "/";
  if (rawUrl.startsWith(`${prefix}/`)) return rawUrl.slice(prefix.length);
  return rawUrl;
}

const server = http.createServer((req, res) => {
  const headers = { ...req.headers };
  for (const header of hopByHop) delete headers[header];
  headers.host = target.host;

  const upstream = http.request(
    {
      hostname: target.hostname,
      port: target.port || 80,
      method: req.method,
      path: upstreamPath(req.url || "/"),
      headers,
    },
    (upstreamRes) => {
      const responseHeaders = { ...upstreamRes.headers };
      for (const header of hopByHop) delete responseHeaders[header];
      res.writeHead(upstreamRes.statusCode || 502, responseHeaders);
      upstreamRes.pipe(res);
    },
  );
  upstream.on("error", (err) => {
    res.writeHead(502, { "content-type": "text/plain" });
    res.end(`proxy error: ${err.message}`);
  });
  req.pipe(upstream);
});

server.listen(listenPort, "127.0.0.1", () => {
  console.error(`proxy listening on 127.0.0.1:${listenPort} -> ${target.href}`);
});
NODE

  node "$WORK/gotrue_prefix_proxy.mjs" "$GOTRUE_PROXY_PORT" "http://127.0.0.1:$GOTRUE_PORT" \
    >"$WORK/gotrue-proxy.log" 2>&1 &
  PIDS+=("$!")
}

start_postgres() {
  local name="$1"
  local port="${2:-}"
  local args=(--name "$name" --network "$NETWORK" -d
    -e POSTGRES_PASSWORD=zeroship
    -e POSTGRES_USER=postgres
    -e POSTGRES_DB=zeroship)
  if [ -n "$port" ]; then
    args+=(-p "127.0.0.1:$port:5432")
  fi
  args+=(postgres:16 -c max_connections=300)
  docker run "${args[@]}" >/dev/null || fail "docker run postgres failed for $name"
  local i
  for i in $(seq 1 45); do
    docker exec "$name" pg_isready -U postgres >/dev/null 2>&1 && return 0
    sleep 1
  done
  fail "$name never became ready"
}

apply_control_migrations() {
  local f
  for f in $(find "$ROOT/db/migrations" -maxdepth 1 -name 'V*.sql' ! -name '*.down.*' | sort); do
    docker exec -i "$CONTROL_PG_CONTAINER" psql -U postgres -d zeroship \
      -v ON_ERROR_STOP=1 -q <"$f" >"$WORK/control-migrate.log" 2>&1 || {
        tail -40 "$WORK/control-migrate.log" || true
        fail "migration $(basename "$f") failed"
      }
  done
  docker exec "$CONTROL_PG_CONTAINER" psql -U postgres -d zeroship -tAc \
    "select to_regclass('zeroship.device_grants')" | grep -q device_grants \
    || fail "control DB missing device_grants after migrations"
  pass "fresh control DB migrated through device/authz tables"
}

start_gotrue() {
  local jwt_env=(
    -e GOTRUE_JWT_SECRET="$JWT_SECRET"
    -e GOTRUE_JWT_ISSUER="$SUPABASE_ISSUER"
    -e GOTRUE_JWT_AUD=authenticated
    -e GOTRUE_JWT_DEFAULT_GROUP_NAME=authenticated
    -e GOTRUE_JWT_ADMIN_GROUP_NAME=service_role
    -e GOTRUE_JWT_ADMIN_ROLES=service_role
    -e GOTRUE_JWT_EXP=3600
  )
  if [ "$SUPABASE_E2E_MODE" = "jwks" ]; then
    jwt_env+=(-e GOTRUE_JWT_KEYS="$GOTRUE_JWT_KEYS")
  fi

  docker exec -i "$GOTRUE_PG_CONTAINER" psql -U postgres -d zeroship \
    -v ON_ERROR_STOP=1 -q >"$WORK/gotrue-schema.log" 2>&1 <<'SQL' || {
CREATE SCHEMA IF NOT EXISTS auth;
ALTER DATABASE zeroship SET search_path TO auth, public;
ALTER ROLE postgres IN DATABASE zeroship SET search_path TO auth, public;
SQL
      tail -40 "$WORK/gotrue-schema.log" || true
      fail "GoTrue auth schema create failed"
    }

  docker run --rm --network "$NETWORK" \
    -e GOTRUE_API_HOST=0.0.0.0 \
    -e GOTRUE_API_PORT=9999 \
    -e GOTRUE_SITE_URL="http://localhost:$GATE_PORT" \
    -e API_EXTERNAL_URL="$SUPABASE_ISSUER" \
    -e GOTRUE_DB_DRIVER=postgres \
    -e GOTRUE_DB_NAMESPACE=auth \
    -e "GOTRUE_DB_DATABASE_URL=postgres://postgres:zeroship@$GOTRUE_PG_CONTAINER:5432/zeroship?sslmode=disable" \
    "${jwt_env[@]}" \
    -e GOTRUE_DISABLE_SIGNUP=false \
    -e GOTRUE_MAILER_AUTOCONFIRM=true \
    -e GOTRUE_EXTERNAL_EMAIL_ENABLED=true \
    "$GOTRUE_IMAGE" auth migrate >"$WORK/gotrue-migrate.log" 2>&1 || {
      tail -80 "$WORK/gotrue-migrate.log" || true
      fail "GoTrue auth migrate failed"
    }
  pass "GoTrue auth schema migrated"

  docker run --name "$GOTRUE_CONTAINER" --network "$NETWORK" -d -p "127.0.0.1:$GOTRUE_PORT:9999" \
    -e GOTRUE_API_HOST=0.0.0.0 \
    -e GOTRUE_API_PORT=9999 \
    -e GOTRUE_SITE_URL="http://localhost:$GATE_PORT" \
    -e API_EXTERNAL_URL="$SUPABASE_ISSUER" \
    -e GOTRUE_DB_DRIVER=postgres \
    -e GOTRUE_DB_NAMESPACE=auth \
    -e "GOTRUE_DB_DATABASE_URL=postgres://postgres:zeroship@$GOTRUE_PG_CONTAINER:5432/zeroship?sslmode=disable" \
    "${jwt_env[@]}" \
    -e GOTRUE_DISABLE_SIGNUP=false \
    -e GOTRUE_MAILER_AUTOCONFIRM=true \
    -e GOTRUE_EXTERNAL_EMAIL_ENABLED=true \
    -e GOTRUE_LOG_LEVEL=info \
    "$GOTRUE_IMAGE" auth serve >/dev/null || fail "docker run GoTrue failed"

  wait_http "http://localhost:$GOTRUE_PORT/health" "GoTrue healthy on raw port $GOTRUE_PORT"
  start_prefix_proxy
  wait_http "$GOTRUE_URL/health" "GoTrue healthy through Supabase /auth/v1 proxy"
  verify_gotrue_jwks
}

start_zeroship_stack() {
  export ZEROSHIP_DEV_INSECURE=1
  export WORKER_KEY
  local control_verify_args=(--supabase-jwt-issuer "$SUPABASE_ISSUER")
  if [ "$SUPABASE_E2E_MODE" = "jwks" ]; then
    control_verify_args+=(--supabase-jwks-url "$SUPABASE_JWKS_URL")
  else
    control_verify_args+=(--supabase-jwt-secret "$JWT_SECRET")
  fi

  "$BIN/zeroship-control" --port "$CONTROL_PORT" --db "$CONTROL_DB_URL" \
    --provision-db "$CONTROL_DB_URL" \
    --blob-store "$WORK/blobs" \
    --control-key "$CONTROL_KEY" \
    --worker-key "$WORKER_KEY" \
    --master-key "$MASTER_KEY" \
    --dev-insecure \
    --auth-provider supabase \
    --supabase-url "$SUPABASE_URL" \
    --supabase-anon-key "$SUPABASE_ANON_KEY" \
    --supabase-service-role-key "$SUPABASE_SERVICE_ROLE_KEY" \
    "${control_verify_args[@]}" \
    --app-base-domain zeroship.localhost \
    >"$WORK/control.log" 2>&1 &
  PIDS+=("$!")
  wait_http "$CONTROL_URL/health" "control healthy with Supabase provider"

  "$BIN/zeroship-worker" --port "$WORKER_PORT" --worker-threads 2 \
    --control "$CONTROL_URL" \
    --control-key "$CONTROL_KEY" \
    --worker-key "$WORKER_KEY" \
    --db "$CONTROL_DB_URL" \
    --blob-store "$WORK/blobs" \
    --poll-interval 2 \
    --dev-insecure \
    >"$WORK/worker.log" 2>&1 &
  PIDS+=("$!")
  wait_http "http://localhost:$WORKER_PORT/health" "worker healthy"

  "$BIN/zeroship-gate" --port "$GATE_PORT" \
    --control "$CONTROL_URL" \
    --control-key "$CONTROL_KEY" \
    --worker-key "$WORKER_KEY" \
    --workers "http://localhost:$WORKER_PORT" \
    --db "$CONTROL_DB_URL" \
    --blob-store "$WORK/blobs" \
    --blob-cache-disk-root "$WORK/blob-cache" \
    --poll-interval 2 \
    --dev-insecure \
    >"$WORK/gate.log" 2>&1 &
  PIDS+=("$!")
  wait_http "$GATE_URL/health" "gateway healthy"
}

refresh_gotrue_session() {
  local refresh_token="$1"
  post_json "$GOTRUE_URL/token?grant_type=refresh_token" \
    "$(jq -nc --arg refresh_token "$refresh_token" '{refresh_token:$refresh_token}')" \
    -H "apikey: $SUPABASE_ANON_KEY" \
    -H "Authorization: Bearer $SUPABASE_ANON_KEY"
}

query_control_db() {
  docker exec "$CONTROL_PG_CONTAINER" psql -U postgres -d zeroship -tAc "$1"
}

step "Preflight"
WORK="$(mktemp -d -t zs-supabase-e2e-XXXXXX)"
mkdir -p "$WORK/blobs" "$WORK/blob-cache"
for cmd in docker curl jq node openssl du find grep paste tail; do
  need_cmd "$cmd"
done
ensure_release_bins
ensure_starter_zship
[ "$SUPABASE_E2E_MODE" = "jwks" ] && echo "  mode: $SUPABASE_E2E_MODE"
echo "  GoTrue image: $GOTRUE_IMAGE"
echo "  ports: gotrue=$GOTRUE_PORT proxy=$GOTRUE_PROXY_PORT control=$CONTROL_PORT worker=$WORKER_PORT gate=$GATE_PORT pg=$CONTROL_PG_PORT"

step "Mint Supabase API keys"
if [ "$SUPABASE_E2E_MODE" = "jwks" ]; then
  generate_jwks_signing_key
  export SUPABASE_JWT_PRIVATE_JWK
fi
SUPABASE_ANON_KEY="$(mint_gotrue_key anon)"
SUPABASE_SERVICE_ROLE_KEY="$(mint_gotrue_key service_role)"
export SUPABASE_ANON_KEY SUPABASE_SERVICE_ROLE_KEY
echo "  anon key:         $(redact_jwt "$SUPABASE_ANON_KEY")"
echo "  service_role key: $(redact_jwt "$SUPABASE_SERVICE_ROLE_KEY")"
echo "  shared issuer:    $SUPABASE_ISSUER"
if [ "$SUPABASE_E2E_MODE" = "jwks" ]; then
  echo "  JWKS URL:         $SUPABASE_JWKS_URL"
  assert_jwt_mode_header "$SUPABASE_ANON_KEY" "anon apikey"
  assert_jwt_mode_header "$SUPABASE_SERVICE_ROLE_KEY" "service_role apikey"
else
  echo "  HS256 secret:     ${JWT_SECRET:0:8}...<redacted>"
fi
pass "minted anon/service_role JWT API keys"

step "Bring up GoTrue + control DB"
docker network create "$NETWORK" >/dev/null || fail "docker network create failed"
docker pull "$GOTRUE_IMAGE" >/dev/null || fail "docker pull $GOTRUE_IMAGE failed"
start_postgres "$CONTROL_PG_CONTAINER" "$CONTROL_PG_PORT"
pass "control Postgres ready on :$CONTROL_PG_PORT"
apply_control_migrations
start_postgres "$GOTRUE_PG_CONTAINER"
pass "GoTrue Postgres ready"
start_gotrue

step "Bring up zeroship control/worker/gateway"
start_zeroship_stack

step "Create and log in a real GoTrue user"
EMAIL="supabase-e2e+$(date +%s)-$RANDOM@example.com"
PASSWORD="Zs-e2e-$(openssl rand -hex 12)!aA1"
SIGNUP="$(post_json "$GOTRUE_URL/signup" \
  "$(jq -nc --arg email "$EMAIL" --arg password "$PASSWORD" '{email:$email,password:$password}')" \
  -H "apikey: $SUPABASE_ANON_KEY" \
  -H "Authorization: Bearer $SUPABASE_ANON_KEY")"
echo "  signup response keys: $(jq -r 'keys | join(",")' <<<"$SIGNUP")"
LOGIN="$(post_json "$GOTRUE_URL/token?grant_type=password" \
  "$(jq -nc --arg email "$EMAIL" --arg password "$PASSWORD" '{email:$email,password:$password}')" \
  -H "apikey: $SUPABASE_ANON_KEY" \
  -H "Authorization: Bearer $SUPABASE_ANON_KEY")"
ACCESS_TOKEN="$(json_get '.access_token' <<<"$LOGIN")"
REFRESH_TOKEN="$(json_get '.refresh_token' <<<"$LOGIN")"
[ -n "$ACCESS_TOKEN" ] || fail "GoTrue password grant returned no access_token: $LOGIN"
[ -n "$REFRESH_TOKEN" ] || fail "GoTrue password grant returned no refresh_token: $LOGIN"
assert_jwt_mode_header "$ACCESS_TOKEN" "GoTrue access token"
GOTRUE_SUB="$(decode_jwt_payload "$ACCESS_TOKEN" | jq -r '.sub')"
GOTRUE_ROLE="$(decode_jwt_payload "$ACCESS_TOKEN" | jq -r '.role')"
GOTRUE_AUD="$(decode_jwt_payload "$ACCESS_TOKEN" | jq -r '.aud')"
echo "  user: $EMAIL"
echo "  sub:  $GOTRUE_SUB"
echo "  session: access=$(redact_jwt "$ACCESS_TOKEN") refresh=${REFRESH_TOKEN:0:10}...<redacted>"
echo "  token claims: role=$GOTRUE_ROLE aud=$GOTRUE_AUD"
[ "$GOTRUE_ROLE" = "authenticated" ] || fail "GoTrue access token role was $GOTRUE_ROLE"
[ "$GOTRUE_AUD" = "authenticated" ] || fail "GoTrue access token aud was $GOTRUE_AUD"
ADMIN_USER="$(curl -fsS "$GOTRUE_URL/admin/users/$GOTRUE_SUB" \
  -H "apikey: $SUPABASE_SERVICE_ROLE_KEY" \
  -H "Authorization: Bearer $SUPABASE_SERVICE_ROLE_KEY")" \
  || fail "service_role admin user lookup failed"
EMAIL_CONFIRMED_AT="$(json_get '.email_confirmed_at' <<<"$ADMIN_USER")"
[ -n "$EMAIL_CONFIRMED_AT" ] || fail "GoTrue admin lookup did not show email_confirmed_at: $ADMIN_USER"
pass "GoTrue user session issued and admin lookup confirms email"

step "Device flow: auth, scripted approve, one-time token poll"
DEVICE="$(post_json "$CONTROL_URL/api/device/auth" \
  '{"client_id":"zeroship-cli","scope":"openid offline_access apps:write apps:deploy apps:read"}')"
DEVICE_CODE="$(json_get '.device_code' <<<"$DEVICE")"
USER_CODE="$(json_get '.user_code' <<<"$DEVICE")"
INTERVAL="$(json_get '.interval' <<<"$DEVICE")"
[ -n "$DEVICE_CODE" ] && [ -n "$USER_CODE" ] || fail "device auth missing codes: $DEVICE"
echo "  device_code: ${DEVICE_CODE:0:10}...<redacted>"
echo "  user_code:   $USER_CODE"
echo "  interval:    ${INTERVAL}s"

APPROVE_BODY="$(jq -nc --arg user_code "$USER_CODE" --arg refresh_token "$REFRESH_TOKEN" \
  '{user_code:$user_code,refresh_token:$refresh_token}')"
APPROVE_OUT="$WORK/device-approve.json"
APPROVE_CODE="$(curl -sS -o "$APPROVE_OUT" -w '%{http_code}' -X POST "$CONTROL_URL/api/device/approve" \
  -H 'Content-Type: application/json' \
  -H "Authorization: Bearer $ACCESS_TOKEN" \
  -d "$APPROVE_BODY" || true)"
[ "$APPROVE_CODE" = "204" ] || fail "device approve returned HTTP $APPROVE_CODE: $(cat "$APPROVE_OUT" 2>/dev/null || true)"
pass "device approval accepted real GoTrue bearer and refresh token"

LINK_COUNT="$(query_control_db "SELECT count(*) FROM zeroship.identity_links WHERE provider = 'supabase' AND provider_subject = '$GOTRUE_SUB'")"
[ "$LINK_COUNT" = "1" ] || fail "expected one identity_link for GoTrue subject, got $LINK_COUNT"
GRANTS="$(query_control_db "SELECT string_agg(grant_name, ',' ORDER BY grant_name) FROM zeroship.principal_grants pg JOIN zeroship.identity_links il ON il.principal_id = pg.principal_id WHERE il.provider = 'supabase' AND il.provider_subject = '$GOTRUE_SUB'")"
echo "  provisioned grants: $GRANTS"
[ "$GRANTS" = "apps:deploy,apps:read,apps:write" ] || fail "unexpected provisioned grants: $GRANTS"
pass "control provisioned and linked the GoTrue principal"

sleep "${INTERVAL:-5}"
DEVICE_TOKEN="$(post_json "$CONTROL_URL/api/device/token" \
  "$(jq -nc --arg device_code "$DEVICE_CODE" \
    '{device_code:$device_code,grant_type:"urn:ietf:params:oauth:grant-type:device_code"}')")"
BOUND_REFRESH="$(json_get '.refresh_token' <<<"$DEVICE_TOKEN")"
BOUND_PROVIDER="$(json_get '.provider' <<<"$DEVICE_TOKEN")"
BOUND_ENDPOINT="$(json_get '.token_endpoint' <<<"$DEVICE_TOKEN")"
[ "$BOUND_PROVIDER" = "supabase" ] || fail "device token provider mismatch: $DEVICE_TOKEN"
[ "$BOUND_REFRESH" = "$REFRESH_TOKEN" ] || fail "device token did not return the approved GoTrue refresh token"
[ "$BOUND_ENDPOINT" = "$GOTRUE_URL/token?grant_type=refresh_token" ] || fail "device token endpoint mismatch: $BOUND_ENDPOINT"
echo "  device poll returned provider=$BOUND_PROVIDER token_endpoint=$BOUND_ENDPOINT"
pass "device token poll returned the one-time GoTrue refresh session"

SECOND_CODE="$(curl -sS -o "$WORK/device-second.json" -w '%{http_code}' -X POST "$CONTROL_URL/api/device/token" \
  -H 'Content-Type: application/json' \
  -d "$(jq -nc --arg device_code "$DEVICE_CODE" \
    '{device_code:$device_code,grant_type:"urn:ietf:params:oauth:grant-type:device_code"}')" || true)"
[ "$SECOND_CODE" = "400" ] || fail "device grant was not one-time; second poll HTTP $SECOND_CODE"
pass "device grant is one-time after redemption"

step "Refresh GoTrue session and deploy with GoTrue bearer"
REFRESHED="$(refresh_gotrue_session "$BOUND_REFRESH")"
DEPLOY_ACCESS_TOKEN="$(json_get '.access_token' <<<"$REFRESHED")"
[ -n "$DEPLOY_ACCESS_TOKEN" ] || fail "GoTrue refresh returned no access_token: $REFRESHED"
assert_jwt_mode_header "$DEPLOY_ACCESS_TOKEN" "refreshed GoTrue access token"
echo "  refreshed access: $(redact_jwt "$DEPLOY_ACCESS_TOKEN")"

DEPLOY_OUT="$("$BIN/zeroship" deploy "$ZSHIP" \
  --app="$APP_NAME" \
  --control="$CONTROL_URL" \
  --token="$DEPLOY_ACCESS_TOKEN" 2>&1)" || fail "zeroship deploy failed: $DEPLOY_OUT"
echo "$DEPLOY_OUT"
DEPLOY_HASH="$(awk '/deploy_hash:/ { print $2 }' <<<"$DEPLOY_OUT" | tail -1)"
[ -n "$DEPLOY_HASH" ] || fail "deploy output did not include deploy_hash: $DEPLOY_OUT"
APP_ID="$(curl -fsS "$CONTROL_URL/api/apps" -H "Authorization: Bearer $DEPLOY_ACCESS_TOKEN" \
  | jq -r --arg name "$APP_NAME" '.[] | select(.name == $name) | .id' | head -1)"
[ -n "$APP_ID" ] || fail "deployed app $APP_NAME not visible to GoTrue bearer"
API_KEY="$(query_control_db "SELECT api_key FROM zeroship.apps WHERE id = '$APP_ID'")"
[ -n "$API_KEY" ] || fail "could not read api_key for app $APP_ID"
echo "  app_id:      $APP_ID"
echo "  deploy_hash: $DEPLOY_HASH"
pass "deployed starter .zship through control using a real GoTrue access token"

step "Gateway serves deployed app"
INDEX=""
for _ in $(seq 1 30); do
  INDEX="$(curl -sS "$GATE_URL/apps/$APP_NAME/" -H "X-Api-Key: $API_KEY" 2>/dev/null || true)"
  if grep -qi '<!doctype html' <<<"$INDEX"; then
    break
  fi
  sleep 1
done
grep -qi '<!doctype html' <<<"$INDEX" || fail "gateway did not serve index.html; got: ${INDEX:0:160}"
pass "GET /apps/$APP_NAME/ serves index.html through gateway"

ASSET="$(grep -oE '/assets/[A-Za-z0-9._-]+\.js' <<<"$INDEX" | head -1 || true)"
if [ -n "$ASSET" ]; then
  ASSET_CODE="$(curl -sS -o /dev/null -w '%{http_code}' "$GATE_URL/apps/$APP_NAME$ASSET" -H "X-Api-Key: $API_KEY" || true)"
  [ "$ASSET_CODE" = "200" ] || fail "gateway asset $ASSET returned HTTP $ASSET_CODE"
  pass "gateway serves client JS asset $ASSET"
fi

echo ""
green "=== supabase deploy e2e: $PASS passed, $FAIL failed ==="
