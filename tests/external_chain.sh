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
// `addUserToHTPasswd`, NOT `generateHtpasswdLine`. This block used to import the
// latter and broke with `TypeError: generateHtpasswdLine is not a function` --
// measured 2026-08-10 against verdaccio/verdaccio:6, whose utils.js exports
// exactly: addUserToHTPasswd, changePasswordToHTPasswd, lockAndRead,
// parseHTPasswd, sanityCheck, stringToUtf8, verifyPassword.
//
// The function did not disappear; it stopped being EXPORTED. `addUserToHTPasswd`
// still calls it internally. That is the whole hazard: this harness reaches into
// a private internal of a container image pinned to a FLOATING tag, so an
// upstream refactor with no API change silently disables the only harness that
// can see the creator-install path. Using the exported entry point does not
// remove the coupling -- it moves it to a surface upstream is likelier to keep.
const utils = require("/usr/local/lib/node_modules/verdaccio/node_modules/verdaccio-htpasswd/build/utils.js");
const { constants } = require("/usr/local/lib/node_modules/verdaccio/node_modules/@verdaccio/core");

(async () => {
  const user = process.env.ZS_PUBLISH_USER;
  const password = process.env.ZS_PUBLISH_PASSWORD;
  if (!user || !password) throw new Error("missing publisher credentials");
  if (typeof utils.addUserToHTPasswd !== "function") {
    throw new Error(
      "verdaccio-htpasswd no longer exports addUserToHTPasswd; exports are: " +
        Object.keys(utils).join(", "),
    );
  }
  let body = "";
  try {
    body = fs.readFileSync(path, "utf8");
  } catch (err) {
    if (err.code !== "ENOENT") throw err;
  }
  // Drop any prior line for this user first: addUserToHTPasswd APPENDS, so a
  // re-run would otherwise stack duplicate entries for the same publisher.
  const kept = body
    .split(/\r?\n/)
    .filter((existing) => existing && !existing.startsWith(`${user}:`))
    .join("\n");
  const next = await utils.addUserToHTPasswd(
    kept ? `${kept}\n` : "",
    user,
    password,
    { algorithm: constants.HtpasswdHashAlgorithm.bcrypt, rounds: 10 },
  );
  fs.writeFileSync(path, next.endsWith("\n") ? next : `${next}\n`, { mode: 0o644 });
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

  # rg exit codes are THREE-VALUED: 0 = matched, 1 = no match, >=2 = ERROR.
  # This used to be spelled `if rg ...; then <fail>; fi; echo "no links"`, which
  # collapses 1 and 2 into the same branch, so a TOOL ERROR read as a clean bill
  # of health.
  #
  # MEASURED under bash on 2026-08-12, three arms differing in one variable:
  #   A. no node_modules/@zeroship/* at all, clean lock
  #        -> glob stays literal, rg exits 2 ("No such file or directory"),
  #           old code printed "no workspace:/file: links in external install"
  #   B. no node_modules/@zeroship/*, but package-lock.json DOES carry
  #      "resolved": "workspace:*"
  #        -> same rg exit 2, old code AGAIN reported no links. A real link
  #           missed, which is the failure this whole function exists to catch.
  #   C. same lock as B, plus node_modules/@zeroship/db/package.json present
  #        -> glob expands, rg exits 0, correctly reported LINKS PRESENT.
  # So the guard went blind exactly when the install was incomplete - the state
  # in which a leaked link is most likely.
  #
  # Two changes: the @zeroship package set must be non-empty (an empty scan is
  # now an error, not a pass), and rg's status is dispatched on all three values.
  local link_targets=(package-lock.json package.json)
  local pkg
  for pkg in node_modules/@zeroship/*/package.json; do
    if [ ! -e "$pkg" ]; then
      echo "no installed @zeroship packages to scan for workspace:/file: links" >&2
      echo "  (node_modules/@zeroship/*/package.json matched nothing - the install" >&2
      echo "   did not produce the packages this check exists to inspect)" >&2
      exit 1
    fi
    link_targets+=("$pkg")
  done

  local rg_status=0
  rg -n 'workspace:|file:' "${link_targets[@]}" >/tmp/zs-external-links.log 2>&1 || rg_status=$?
  case "$rg_status" in
    0)
      cat /tmp/zs-external-links.log >&2
      echo "external install contains workspace:/file: links" >&2
      exit 1
      ;;
    1)
      echo "no workspace:/file: links in external install (scanned ${#link_targets[@]} files)"
      ;;
    *)
      cat /tmp/zs-external-links.log >&2
      echo "link scan FAILED (rg exit $rg_status); refusing to report it as clean" >&2
      exit 1
      ;;
  esac
}

require_local_registry

# compose now requires generated secrets to interpolate; idempotent.
. "$(dirname "$0")/lib/dev_secrets.sh"
ensure_dev_secrets || exit 1

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

log "5. Apply the scaffolded app's migrations"
# THE ONLY PLACE THIS CAN BE TESTED. The template declares
# `"migrate": "zeroship-dev-migrate"`, the bin comes from @zeroship/vite-plugin,
# and `migrations/20260628000000_initial_schema.ts` ships in the scaffold. Every
# link reads correctly -- and until now NOTHING ran it, so all five were
# spelling rather than behaviour (docs/pilot/e2e-scenarios.md, scenario 1).
#
# golden_path.sh cannot cover this: it runs inside the monorepo, where the bin
# resolves through workspace linking, so the check would pass even if the
# PUBLISHED package were broken -- which is the only failure a creator can hit.
# Its own migrate step sidesteps the bin deliberately, invoking
# `node <dist>/cli/migrate-dev.js` by path. Here the app was installed from the
# registry, outside the tree, so `npm run migrate` exercises what a creator
# actually types.
npm run migrate
test -f .zeroship/dev.sqlite || { echo "FAIL: migrate produced no dev database"; exit 1; }
echo "  scaffolded app migrated; dev database exists"

log "6. Build external app"
npm run build
test -f dist/app.zship
ls -lh dist/app.zship

log "external chain: PASS"
echo "Verdaccio is left running under docker compose (service: verdaccio)."
