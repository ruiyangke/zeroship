---
name: zeroship-deploy
description: Use when building, deploying, or migrating a zeroship app, when choosing between pnpm dev and a real deploy, or when a deployed app serves its pages but fails on its first database call. Covers the build artifact, zeroship.jsonc, and the deploy-then-migrate sequence.
---

# Build, deploy, migrate

## Local development

```bash
pnpm install
pnpm dev
```

`pnpm dev` runs the app against a project-local SQLite database and a local
key-value store under `.zeroship/`. No platform services and no Postgres are
needed. That directory is persistent dev state; leave it between runs.

**`pnpm dev` runs no gateway.** Auth policy, rate limits and CSRF checks are not
enforced locally. An app can be perfectly green in dev and still return 401 for
every caller once deployed. See `zeroship-rpc`.

## Build

```bash
pnpm build      # vite build -> dist/app.zship
```

The plugin discovers the `"use server"` procedures, folds `migrations/` into the
generated artifacts, bundles the server module and the static client, and writes
the deploy artifact. A production build refuses to continue when a procedure has
no explicit id, or when the committed artifacts under `generated/zeroship/`
drift from the migration source.

## Deploy, then migrate

```bash
zeroship login      # once, device flow
zeroship deploy     # upload dist/app.zship
zeroship migrate    # apply the schema
```

Neither command needs a target. The artifact path, the app and the control
plane come from `zeroship.jsonc`, and each command prints what it resolved and
from where before acting. `--app=<id>` and `--control=<url>` still override the
file. `zeroship config show` prints the resolved configuration.

On the first deploy `zeroship.jsonc` has no `app` key: deploy falls back to the
project `name`, creates that app, and writes its id back into the file.
`migrate`, `secret` and `var` do not take that fallback, so run `deploy` first.

### Run both, in that order, whenever the schema changes

This is the single most common deployment failure. The artifact carries the
app's code and the generated schema typing. It does **not** carry the
migrations, and deploying does not apply them. `zeroship migrate` reads this
app's `migrations/*.ts`, records them, and posts the result to the migration
service, which creates the app's schema, its tables, and the per-app database
role the runtime assumes on every `env.db` call. Run it from the app directory
with `node_modules` installed: the recording is the same one the build does.

Skip it and the app deploys clean, serves its static assets, dispatches its
RPCs, and then fails the first database call because that role does not exist.
The end user sees `{"message":"internal error"}`.

Re-running `zeroship migrate` with nothing new to apply is a no-op, so running
it after every deploy is safe.

## Configuration and secrets

`zeroship.jsonc` names the app, the control plane, the build output and the
migration paths, so every tool reads one spelling of them. Named `environments`
select different targets.

```bash
zeroship secret set STRIPE_KEY=sk_live_... --app=<id>
zeroship var set LOG_LEVEL=debug --app=<id>
```

Secrets are encrypted at rest and always readable as `env.KEY`. They reach
`process.env`, where any npm dependency can read them, only when explicitly
exposed. Prefer `env.KEY`.
