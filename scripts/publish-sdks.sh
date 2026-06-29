#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REGISTRY="${ZEROSHIP_NPM_REGISTRY:-http://localhost:4873}"

NPM_USERCONFIG_ARGS=()
if [[ -n "${NPM_CONFIG_USERCONFIG:-}" ]]; then
  NPM_USERCONFIG_ARGS=(--userconfig "$NPM_CONFIG_USERCONFIG")
fi

publish_packages=(
  "sdks/types"
  "sdks/db"
  "sdks/migrate"
  "sdks/bootstrap"
  "sdks/auth"
  "sdks/kv"
  "sdks/storage"
  "sdks/migrations"
  "sdks/rpc"
  "sdks/server"
  "sdks/react"
  "sdks/ui"
  "sdks/payments"
  "sdks/eslint-config"
  "sdks/vite-plugin"
  "sdks/create-zeroship-app"
)

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
