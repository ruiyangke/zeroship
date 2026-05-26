# Private SDK Registry

Phase 1 runs a Verdaccio registry for publishing and consuming ZeroShip SDK packages during local and compose development. The registry listens on `http://localhost:4873`, serves `@zeroship/*` packages from local storage, and proxies everything else to npmjs.

## Start Verdaccio Locally

From the repo root:

```bash
pnpm dlx verdaccio --config config/verdaccio/config.yaml
```

The committed config writes local state under `config/verdaccio/storage/`, which is ignored by git.

## Start Verdaccio In Compose

```bash
docker compose up -d verdaccio
docker compose ps verdaccio
curl -fsS http://localhost:4873/-/ping
```

The compose service persists registry state in the `verdaccio_storage` volume and uses the same committed config at `config/verdaccio/config.yaml`.

## Create A Publish User

Publishing is restricted to authenticated users; anonymous package reads are allowed.

```bash
REGISTRY=http://localhost:4873
NPM_CONFIG_USERCONFIG="$(pwd)/.verdaccio-npmrc"
export REGISTRY NPM_CONFIG_USERCONFIG

npm adduser --registry "$REGISTRY" --auth-type=legacy --userconfig "$NPM_CONFIG_USERCONFIG"
npm whoami --registry "$REGISTRY" --userconfig "$NPM_CONFIG_USERCONFIG"
```

Keep `.verdaccio-npmrc` local. Do not commit publish tokens.

For non-interactive smoke tests, create the same user through Verdaccio's npm-compatible user endpoint and write the returned token into a temporary npmrc:

```bash
REGISTRY=http://localhost:4873
NPM_CONFIG_USERCONFIG="$(mktemp)"
TOKEN="$(
  curl -fsS -X PUT "$REGISTRY/-/user/org.couchdb.user:zeroship-publisher" \
    -H 'content-type: application/json' \
    --data '{"name":"zeroship-publisher","password":"zeroship-publisher-password","email":"zeroship@example.com","type":"user","roles":[]}' |
    node -e 'const fs = require("node:fs"); process.stdout.write(JSON.parse(fs.readFileSync(0, "utf8")).token)'
)"
printf 'registry=%s/\n@zeroship:registry=%s/\n//localhost:4873/:_authToken=%s\nalways-auth=true\n' "$REGISTRY" "$REGISTRY" "$TOKEN" > "$NPM_CONFIG_USERCONFIG"
export NPM_CONFIG_USERCONFIG
```

## Publish SDKs

```bash
ZEROSHIP_NPM_REGISTRY=http://localhost:4873 \
NPM_CONFIG_USERCONFIG="$(pwd)/.verdaccio-npmrc" \
pnpm publish:sdks
```

The script builds the SDK workspace first, then publishes the SDK package set in dependency order. If the same version is already present, it unpublishes that exact version and publishes it again so repeated pre-launch local runs are deterministic.

Published packages:

- `@zeroship/types`
- `@zeroship/db`
- `@zeroship/bootstrap`
- `@zeroship/auth`
- `@zeroship/kv`
- `@zeroship/storage`
- `@zeroship/migrations`
- `@zeroship/rpc`
- `@zeroship/server`
- `@zeroship/react`
- `@zeroship/ui`
- `@zeroship/payments`
- `@zeroship/eslint-config`
- `@zeroship/vite-plugin`
- `create-zeroship-app`

Skipped package:

- `zeroship` from `sdks/zeroship-stub`: a Node-side test shim for the runtime's virtual `zeroship` module, not a creator-consumable SDK package.

## Verify Registry Resolution

```bash
npm view @zeroship/ui version --registry http://localhost:4873
npm view @zeroship/db version --registry http://localhost:4873
npm view @zeroship/rpc version --registry http://localhost:4873
```

Then verify from outside the monorepo so pnpm cannot use workspace links:

```bash
TMPDIR="$(mktemp -d)"
cd "$TMPDIR"
printf '{"name":"zeroship-registry-smoke","version":"0.0.0","private":true,"type":"module"}\n' > package.json
printf '@zeroship:registry=http://localhost:4873\n' > .npmrc
pnpm add @zeroship/ui
node --input-type=module -e "console.log(import.meta.resolve('@zeroship/ui'))"
```

The resolved path should point inside the throwaway directory's `node_modules`, not back into the monorepo.

## Consumer Configuration

Generated apps only need scoped read access:

```ini
@zeroship:registry=http://localhost:4873
```

Do not put publish credentials in generated apps or sandbox workspaces. Sandbox `.npmrc` wiring and backend reachability are Phase 2.
