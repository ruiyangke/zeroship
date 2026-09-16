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

Generated apps only need scoped read access. The Builder writes this into the
generated app workspace as `.npmrc` whenever `ZEROSHIP_SDK_REGISTRY` is set:

```ini
@zeroship:registry=<ZEROSHIP_SDK_REGISTRY>
```

Do not put publish credentials in generated apps or sandbox workspaces. The
workspace `.npmrc` intentionally sets only the `@zeroship` scope; the default
registry remains npmjs so public dependencies such as React, Vite, and Base UI
continue to resolve normally.

Local Builder env:

```bash
ZEROSHIP_SDK_REGISTRY=http://host.docker.internal:4873
```

The Docker sandbox backend injects `host.docker.internal` with Docker's
`host-gateway` mapping, so this URL reaches either a host-local Verdaccio
process or the compose Verdaccio service published on `localhost:4873`.

Compose-network option, if you intentionally want Docker DNS instead of the
host-gateway route:

```bash
docker network connect zeroship-sandbox-net "$(docker compose -f deploy/compose/docker-compose.yml ps -q verdaccio)"
ZEROSHIP_SDK_REGISTRY=http://verdaccio:4873
```

Use this URL only after the Verdaccio container has been attached to
`zeroship-sandbox-net`, the same network used by Docker-backed sandbox
containers. The committed compose file does not attach Verdaccio to that named
network by default because local sandbox harnesses often create it outside
compose; Docker Compose refuses to take ownership of that pre-existing network.

## Backend Registry URLs

| Backend | Reachable `ZEROSHIP_SDK_REGISTRY` | Status |
| --- | --- | --- |
| Docker local | `http://host.docker.internal:4873` | Implemented via `--add-host host.docker.internal:host-gateway` in the Docker backend. |
| Docker compose network | `http://verdaccio:4873` | Documented optional route. Manually attach the Verdaccio container to `zeroship-sandbox-net` if you prefer compose DNS. |
| Kubernetes | `http://verdaccio.<namespace>.svc.cluster.local:4873` or the registry Service DNS used by the cluster | Documented. Configure the Builder env to the Service DNS reachable from sandbox pods. |
| Nomad + Cloud Hypervisor | A routable registry URL from the microVM network, for example `http://<registry-host-or-vip>:4873` | Documented. The VM route/firewall must allow egress to the registry host; no backend code change in Phase 2. |

## Sandbox Acceptance Test

The end-to-end acceptance harness that drove a real Docker sandbox against this
registry left with the sandbox backend: both it and the sandbox bring-up script
it called now live in the standalone `zeroship-sandbox` project. Run it from
that checkout, pointed at the Verdaccio started above.

What it covers there, for reference: publish the SDKs to Verdaccio, create a
sandbox through the real backend client, and run `pnpm install` plus `pnpm build`
for a minimal Vite app importing from `@zeroship/ui` - asserting that the sandbox
workspace `.npmrc` carries only the scoped `@zeroship` line, that
`pnpm config get @zeroship:registry` resolves to Verdaccio, and that the build
output contains ZeroShip UI classes such as `zs-theme-root` or `zs-button`.

The registry side of that flow is exercisable from this repo alone with the
`pnpm publish:packages` and throwaway-consumer steps above.
