#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
REGISTRY="${ZEROSHIP_NPM_REGISTRY:-http://localhost:4873}"

NPM_USERCONFIG_ARGS=()
if [[ -n "${NPM_CONFIG_USERCONFIG:-}" ]]; then
  NPM_USERCONFIG_ARGS=(--userconfig "$NPM_CONFIG_USERCONFIG")
fi

publish_packages=(
  "sdks/types"
  "sdks/control"
  "sdks/mcp"
  "sdks/db"
  "sdks/migrate"
  "sdks/bootstrap"
  "sdks/auth"
  "sdks/kv"
  "sdks/storage"
  "sdks/rpc"
  "sdks/server"
  "sdks/react"
  "sdks/ui"
  "sdks/payments"
  "sdks/eslint-config"
  "sdks/vite-plugin"
  "sdks/create-zeroship-app"
)

# This list is hand-maintained and does not track the tree. When
# `chore(reorg): retire @zeroship/migrations SDK` (4feee8f64) deleted
# sdks/migrations it left the entry here, and the next run of
# tests/external_chain.sh died mid-publish with a bare
# `ENOENT: .../sdks/migrations/package.json` thrown from inside json_field --
# the path but not the reason. Fail up front, naming the stale entry.
#
# This checks only that a listed directory EXISTS. It cannot see the other
# direction: a publishable package present in sdks/ and absent from this list
# is still silently unpublished (@zeroship/workflows is in that state today).
missing_packages=()
for package_dir in "${publish_packages[@]}"; do
  [ -f "$ROOT_DIR/$package_dir/package.json" ] || missing_packages+=("$package_dir")
done
if [ ${#missing_packages[@]} -gt 0 ]; then
  echo "[publish-sdks] publish list names ${#missing_packages[@]} package(s) that do not exist:" >&2
  printf '  %s/package.json\n' "${missing_packages[@]}" >&2
  echo "[publish-sdks] the tree moved and this list did not; fix the list" >&2
  exit 1
fi

# The other direction, which the check above cannot see: a package we publish
# may DEPEND on a workspace member we do not publish. `pnpm pack` rewrites
# `workspace:*` to a concrete version, so the tarball ships a dependency on a
# version of a package that exists nowhere the installer can reach. Inside the
# monorepo it resolves by workspace linking and looks fine.
#
# This is not hypothetical. Measured 2026-08-10 via tests/external_chain.sh:
# published @zeroship/vite-plugin@0.3.0 declares zero-migrate@0.1.0 and
# zero-migrate-node@0.1.0, neither of which exists on registry.npmjs.org at ANY
# version, so `npm install` in a scaffolded app dies with E404. The scaffold
# template depends on @zeroship/vite-plugin, so this is every new app.
#
# Optional peers are exempt: npm 7+ does not auto-install them.
node -e '
const fs = require("node:fs");
const dirs = process.argv.slice(1);
const published = new Set(
  dirs.map((d) => JSON.parse(fs.readFileSync(`${d}/package.json`, "utf8")).name),
);
const bad = [];
for (const d of dirs) {
  const pkg = JSON.parse(fs.readFileSync(`${d}/package.json`, "utf8"));
  for (const field of ["dependencies", "peerDependencies", "optionalDependencies"]) {
    for (const [dep, range] of Object.entries(pkg[field] || {})) {
      if (!String(range).startsWith("workspace:")) continue;
      if (published.has(dep)) continue;
      if (field === "peerDependencies" && pkg.peerDependenciesMeta?.[dep]?.optional) continue;
      bad.push(`${pkg.name} [${field}] ${dep}@${range}`);
    }
  }
}
if (bad.length > 0) {
  console.error(`[publish-sdks] ${bad.length} dependency(ies) on workspace packages that are not published:`);
  for (const line of bad) console.error(`  ${line}`);
  console.error("[publish-sdks] pnpm pack will rewrite these to concrete versions that resolve nowhere");
  process.exit(1);
}
' "${publish_packages[@]/#/$ROOT_DIR/}"

json_field() {
  local package_json="$1"
  local field="$2"
  node -e '
    const fs = require("node:fs");
    const pkg = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
    const value = pkg[process.argv[2]];
    if (value === undefined || value === null) process.exit(1);
    process.stdout.write(String(value));
  ' "$package_json" "$field"
}

pack_filename() {
  node -e '
    const fs = require("node:fs");
    const json = JSON.parse(fs.readFileSync(0, "utf8"));
    if (!json.filename) throw new Error("pnpm pack did not return a filename");
    process.stdout.write(json.filename);
  '
}

echo "[publish-sdks] registry: $REGISTRY"
echo "[publish-sdks] packages: ${publish_packages[*]}"
echo "[publish-sdks] skipped: sdks/zeroship-stub (runtime/test shim for the virtual \"zeroship\" module)"

npm ping --registry "$REGISTRY" "${NPM_USERCONFIG_ARGS[@]}" >/dev/null
publisher="$(npm whoami --registry "$REGISTRY" "${NPM_USERCONFIG_ARGS[@]}")"
echo "[publish-sdks] authenticated as: $publisher"

echo "[publish-sdks] building workspace SDKs"
pnpm --dir "$ROOT_DIR" build

pack_dir="$(mktemp -d)"
trap 'rm -rf "$pack_dir"' EXIT

for package_dir in "${publish_packages[@]}"; do
  package_json="$ROOT_DIR/$package_dir/package.json"
  name="$(json_field "$package_json" name)"
  version="$(json_field "$package_json" version)"
  spec="$name@$version"

  if npm view "$spec" version --registry "$REGISTRY" "${NPM_USERCONFIG_ARGS[@]}" >/dev/null 2>&1; then
    echo "[publish-sdks] $spec already exists; unpublishing before re-publish"
    npm unpublish "$spec" --force --registry "$REGISTRY" "${NPM_USERCONFIG_ARGS[@]}"
  fi

  echo "[publish-sdks] packing $spec from $package_dir"
  tarball="$(pnpm --dir "$ROOT_DIR/$package_dir" pack --pack-destination "$pack_dir" --json | pack_filename)"
  echo "[publish-sdks] publishing $spec from $tarball"
  npm publish "$tarball" --registry "$REGISTRY" "${NPM_USERCONFIG_ARGS[@]}"
done

echo "[publish-sdks] done"
