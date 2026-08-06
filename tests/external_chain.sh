#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# External build-local chain proof (gap #1: SDK distribution).
#
# Proves an app OUTSIDE this monorepo can install @zeroship/* from the local
# Verdaccio registry, then run the real Vite plugin build to produce
# dist/app.zship. Publishing is strictly local: this script refuses any registry
# other than localhost/127.0.0.1.
# ---------------------------------------------------------------------------
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
export COMPOSE_FILE="$ROOT/deploy/compose/docker-compose.yml"

REGISTRY="${ZEROSHIP_NPM_REGISTRY:-http://localhost:4873}"
PUBLISH_USER="${ZEROSHIP_NPM_PUBLISH_USER:-zeroship-publisher}"
PUBLISH_PASSWORD="${ZEROSHIP_NPM_PUBLISH_PASSWORD:-zeroship-publisher}"
EXT_ROOT="${ZEROSHIP_EXTERNAL_CHAIN_TMP:-/tmp/zs-ext}"
APP_NAME="${ZEROSHIP_EXTERNAL_CHAIN_APP:-ext-app}"
TMP_NPMRC=""

cleanup() {
  [ -n "$TMP_NPMRC" ] && rm -f "$TMP_NPMRC"
}
trap cleanup EXIT

log() {
  printf '\n=== %s ===\n' "$1"
}

require_local_registry() {
  node -e '
    const registry = new URL(process.argv[1]);
    if (registry.protocol !== "http:" || !["localhost", "127.0.0.1"].includes(registry.hostname)) {
      console.error(`refusing non-local registry: ${registry.href}`);
      process.exit(1);
    }
  ' "$REGISTRY"
}

registry_auth_prefix() {
  node -e '
    const registry = new URL(process.argv[1]);
    let path = registry.pathname || "/";
    if (!path.endsWith("/")) path += "/";
    process.stdout.write(`//${registry.host}${path}`);
  ' "$REGISTRY"
}

wait_for_registry() {
  for _ in $(seq 1 60); do
    if npm ping --registry "$REGISTRY" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  echo "registry did not answer npm ping: $REGISTRY" >&2
  return 1
}

provision_publisher() {
  docker compose exec -T \
    -e ZS_PUBLISH_USER="$PUBLISH_USER" \
    -e ZS_PUBLISH_PASSWORD="$PUBLISH_PASSWORD" \
    verdaccio sh -lc 'node <<'"'"'NODE'"'"'
const fs = require("node:fs");
const path = "/verdaccio/conf/storage/htpasswd";
const { generateHtpasswdLine } = require("/usr/local/lib/node_modules/verdaccio/node_modules/verdaccio-htpasswd/build/utils.js");
const { constants } = require("/usr/local/lib/node_modules/verdaccio/node_modules/@verdaccio/core");

(async () => {
  const user = process.env.ZS_PUBLISH_USER;
  const password = process.env.ZS_PUBLISH_PASSWORD;
  if (!user || !password) throw new Error("missing publisher credentials");
  const line = await generateHtpasswdLine(user, password, {
    algorithm: constants.HtpasswdHashAlgorithm.bcrypt,
    rounds: 10,
  });
  let body = "";
  try {
    body = fs.readFileSync(path, "utf8");
  } catch (err) {
    if (err.code !== "ENOENT") throw err;
  }
  const lines = body
    .split(/\r?\n/)
    .filter((existing) => existing && !existing.startsWith(`${user}:`));
  lines.push(line.trimEnd());
  fs.writeFileSync(path, `${lines.join("\n")}\n`, { mode: 0o644 });
  console.log(`provisioned ${user} in ${path}`);
})().catch((err) => {
  console.error(err);
  process.exit(1);
});
NODE'
}

write_publish_npmrc() {
  TMP_NPMRC="$(mktemp)"
  local auth auth_prefix
  auth="$(printf '%s:%s' "$PUBLISH_USER" "$PUBLISH_PASSWORD" | base64 | tr -d '\n')"
  auth_prefix="$(registry_auth_prefix)"
  {
    printf 'registry=https://registry.npmjs.org/\n'
    printf '@zeroship:registry=%s\n' "$REGISTRY"
    printf '%s:_auth=%s\n' "$auth_prefix" "$auth"
    printf '%s:always-auth=true\n' "$auth_prefix"
    printf 'email=%s@example.invalid\n' "$PUBLISH_USER"
  } >"$TMP_NPMRC"
}

assert_external_resolutions() {
  node - "$REGISTRY" <<'NODE'
const fs = require("node:fs");
const registry = process.argv[2].replace(/\/$/, "");
const lock = JSON.parse(fs.readFileSync("package-lock.json", "utf8"));
const required = new Set([
  "@zeroship/db",
  "@zeroship/kv",
  "@zeroship/migrate",
  "@zeroship/rpc",
  "@zeroship/server",
  "@zeroship/storage",
  "@zeroship/types",
  "@zeroship/vite-plugin",
]);
const seen = new Set();
const bad = [];

for (const [path, meta] of Object.entries(lock.packages)) {
  if (!path.startsWith("node_modules/@zeroship/")) continue;
  const name = path.slice("node_modules/".length);
  seen.add(name);
  const resolved = meta.resolved || "";
  console.log(`${path} ${meta.version} ${resolved}`);
  if (!resolved.startsWith(`${registry}/`)) {
    bad.push(`${name} resolved from ${resolved || "<missing>"}`);
  }
  if (/^(workspace:|file:)/.test(resolved)) {
    bad.push(`${name} leaked local link ${resolved}`);
  }
}

for (const name of required) {
  if (!seen.has(name)) bad.push(`missing ${name} from external package-lock`);
}

if (bad.length > 0) {
  for (const line of bad) console.error(line);
  process.exit(1);
}
NODE

  if rg -n 'workspace:|file:' package-lock.json package.json node_modules/@zeroship/*/package.json >/tmp/zs-external-links.log 2>&1; then
    cat /tmp/zs-external-links.log >&2
    echo "external install contains workspace:/file: links" >&2
    exit 1
  fi
  echo "no workspace:/file: links in external install"
}

require_local_registry

log "1. Verdaccio up"
docker compose up -d verdaccio
wait_for_registry
docker compose ps verdaccio
npm ping --registry "$REGISTRY"

log "2. Publisher account"
provision_publisher
write_publish_npmrc
npm whoami --registry "$REGISTRY" --userconfig "$TMP_NPMRC"

log "3. Build + publish SDKs to local Verdaccio"
pnpm install
pnpm build
NPM_CONFIG_USERCONFIG="$TMP_NPMRC" ZEROSHIP_NPM_REGISTRY="$REGISTRY" "$ROOT/deploy/scripts/publish-sdks.sh"
printf 'npm view @zeroship/rpc version: '
npm view @zeroship/rpc version --registry "$REGISTRY"
printf 'npm view @zeroship/migrate version: '
npm view @zeroship/migrate version --registry "$REGISTRY"

log "4. Scaffold + install outside the monorepo"
rm -rf "$EXT_ROOT"
mkdir -p "$EXT_ROOT"
(cd "$EXT_ROOT" && NPM_CONFIG_CACHE="$EXT_ROOT/.npm-cache" npm exec --yes --prefer-online \
  --registry "$REGISTRY" \
  --package create-zeroship-app@0.1.0 \
  -- create-zeroship-app "$APP_NAME")

cd "$EXT_ROOT/$APP_NAME"
printf '@zeroship:registry=%s\n' "$REGISTRY" > .npmrc
npm install
npm ls @zeroship/db @zeroship/kv @zeroship/migrate @zeroship/rpc @zeroship/server @zeroship/storage @zeroship/types @zeroship/vite-plugin --all
assert_external_resolutions

log "5. Build external app"
npm run build
test -f dist/app.zship
ls -lh dist/app.zship

log "external chain: PASS"
echo "Verdaccio is left running under docker compose (service: verdaccio)."
