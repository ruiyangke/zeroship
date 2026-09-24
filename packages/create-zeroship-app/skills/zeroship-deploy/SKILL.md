---
name: zeroship-deploy
description: Use when building, deploying, or migrating a zeroship app, when choosing between pnpm dev and a real deploy, or when a deployed app serves its pages but fails on its first database call. Covers the build artifact, zeroship.jsonc, and the two independent commands that ship code and apply schema.
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

## Deploy and migrate

```bash
zeroship login      # once, device flow
zeroship deploy     # upload dist/app.zship
zeroship migrate    # apply the schema
```

Neither command needs a target. The artifact path, the app, the database and
the control plane come from `zeroship.jsonc`, and each command prints what it
resolved and from where before acting. `deploy` addresses an APP: with one
declared `--app` is optional, with several it names which label. `migrate`
addresses a DATABASE: with one declared `--database` is optional, with several
it names which label. `--control=<url>` still overrides the file, and
`zeroship config show` prints the resolved configuration.

On the first deploy the `apps` entry has no `app` id: deploy falls back to the
workspace `name`, creates that app, and writes its id back into that entry.
`secret` and `var` do not take that fallback, so run `deploy` before them.

### The two are independent; run migrate whenever the schema changes

This is the single most common deployment failure. The artifact carries the
app's code and the generated schema typing. It does **not** carry the
migrations, and deploying does not apply them. `zeroship migrate` reads the
database's `migrations/*.ts`, records them, and posts the result to the
migration service, which creates the schema and its tables. Run it from the app
directory with `node_modules` installed: the recording is the same one the
build does.

**There is no ordering constraint.** A migration is authorized at the
database's own project, not through an app, so `migrate` needs no prior deploy
and reaches a database no app is bound to.

Skip it and the app deploys clean, serves its static assets, dispatches its
RPCs, and then fails the first database call because the table or column is not
there. The runtime classifies that as `schema_not_migrated` and the response
names `zeroship migrate`, so the end user sees the remedy rather than
`{"message":"internal error"}`.

Re-running `zeroship migrate` with nothing new to apply is a no-op, so running
it after every deploy is safe.

## Configuration and secrets

`zeroship.jsonc` names the workspace's apps and databases by LOCAL LABEL, the
control plane and the build output, so every tool reads one spelling of them.
Each database entry carries its own `migrations` and `out` paths, and each app
entry names the database labels it uses and which is its `primary` - the one
`env.db` reaches. Named `environments` select different targets, and each must
state `apps`, `control` and `databases` in full.

```bash
zeroship secret set STRIPE_KEY=sk_live_... --app=<label>
zeroship var set LOG_LEVEL=debug --app=<label>
```

Secrets are encrypted at rest and always readable as `env.KEY`. They reach
`process.env`, where any npm dependency can read them, only when explicitly
exposed. Prefer `env.KEY`.
