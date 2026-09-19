# Private SDK Registry

Phase 1 runs a Verdaccio registry for publishing and consuming ZeroShip SDK packages during local and compose development. The registry listens on `http://localhost:4873`, serves `@zeroship/*` packages from local storage, and proxies everything else to npmjs.

## Start Verdaccio Locally

From the repo root:

```bash
pnpm dlx verdaccio --config deploy/verdaccio/config.yaml
```

The committed config writes local state under `deploy/verdaccio/storage/`, which is ignored by git.

## Start Verdaccio In Compose

```bash
docker compose -f deploy/compose/docker-compose.yml up -d verdaccio
docker compose -f deploy/compose/docker-compose.yml ps verdaccio
curl -fsS http://localhost:4873/-/ping
```

The compose service persists registry state in the `verdaccio_storage` volume and uses the same committed config at `deploy/verdaccio/config.yaml`.

## Create A Publish User

Publishing is restricted to the named `zeroship-publisher` account; anonymous
package reads are allowed. The committed config
(`deploy/verdaccio/config.yaml`) sets `auth.htpasswd.max_users: -1`, which
DISABLES self-registration (SEC-8), so both the `npm adduser` step below and the
`PUT /-/user/...` endpoint are REFUSED. Provision the publisher out of band
first: write the `zeroship-publisher` entry into
`deploy/verdaccio/storage/htpasswd` (the operator-provisioned path the config's
SEC-8 comment describes), then use `npm adduser` only to cache that existing
account's token.

```bash
REGISTRY=http://localhost:4873
NPM_CONFIG_USERCONFIG="$(pwd)/.verdaccio-npmrc"
export REGISTRY NPM_CONFIG_USERCONFIG

# caches a token for the ALREADY provisioned zeroship-publisher account;
# does not create it (self-registration is disabled).
npm adduser --registry "$REGISTRY" --auth-type=legacy --userconfig "$NPM_CONFIG_USERCONFIG"
npm whoami --registry "$REGISTRY" --userconfig "$NPM_CONFIG_USERCONFIG"
```

Keep `.verdaccio-npmrc` local. Do not commit publish tokens.

For non-interactive smoke tests, write the returned token for the same
out-of-band-provisioned user into a temporary npmrc (the account must already
exist in the htpasswd file — the `PUT` below is refused under
`max_users: -1`):

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
pnpm publish:packages
```

The script builds the SDK workspace first, then publishes the SDK package set in dependency order. If the same version is already present, it unpublishes that exact version and publishes it again so repeated pre-launch local runs are deterministic.

Published packages:

- `@zeroship/types`
- `@zeroship/db`
- `@zeroship/auth`
- `@zeroship/kv`
- `@zeroship/storage`
- `@zeroship/rpc`
- `@zeroship/server`
- `@zeroship/react`
- `@zeroship/payments`
- `@zeroship/eslint-config`
- `@zeroship/vite-plugin`
- `create-zeroship-app`

Skipped package:

- `zeroship` from `packages/zeroship-stub`: a Node-side test shim for the runtime's virtual `zeroship` module, not a creator-consumable SDK package.

## Verify Registry Resolution

```bash
npm view @zeroship/payments version --registry http://localhost:4873
npm view @zeroship/db version --registry http://localhost:4873
npm view @zeroship/rpc version --registry http://localhost:4873
```

Then verify from outside the monorepo so pnpm cannot use workspace links:

```bash
TMPDIR="$(mktemp -d)"
cd "$TMPDIR"
printf '{"name":"zeroship-registry-smoke","version":"0.0.0","private":true,"type":"module"}\n' > package.json
printf '@zeroship:registry=http://localhost:4873\n' > .npmrc
pnpm add @zeroship/payments
node --input-type=module -e "console.log(import.meta.resolve('@zeroship/payments'))"
```

The resolved path should point inside the throwaway directory's `node_modules`, not back into the monorepo.

## Generated App Consumption

Generated apps only need scoped read access. A generated app's workspace
`.npmrc` carries the scope whenever `ZEROSHIP_SDK_REGISTRY` is set:

```ini
@zeroship:registry=<ZEROSHIP_SDK_REGISTRY>
```

Do not put publish credentials in generated apps. The workspace `.npmrc`
intentionally sets only the `@zeroship` scope; the default registry remains
npmjs so public dependencies such as React, Vite, and Base UI continue to
resolve normally.
