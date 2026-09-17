# Environment variables

This page is what your app can read: the variables and secrets you configure for
an app, the identifiers the platform provides, and what a local development run
supplies. It is [what your app can read](#what-your-app-can-read) and
[local development](#local-development).

If a name is not under [what your app can read](#what-your-app-can-read), your
app cannot rely on it. The platform's own service configuration — the names the
API, gateway, worker and auth services parse, and the operator's deployment
surface — is not a creator contract; it is published in
[platform environment variables](../architecture/platform-env-vars.md) so you can
recognize it, not so app code can read it.

---

## What your app can read

Every app - under `pnpm dev`, under `zeroship serve`, and deployed - reads its
environment through the same three surfaces: the `env` object, the
`globalThis.env.get` accessor, and `process.env`. Which values appear on which
surface is the contract.

### Application variables

An application variable is a plaintext name/value pair on an app:

```bash
zeroship var set PUBLIC_URL=https://example.com --app=<app-id>
```

It is visible in both the `env` object and `process.env`. Because it is stored
in plaintext and reaches `process.env`, never put a credential in one.
`zeroship var list --app=<app-id>` prints the names; `zeroship var rm NAME
--app=<app-id>` removes one.

### Secrets

A secret is a name/value pair stored encrypted at rest:

```bash
zeroship secret set OPENAI_API_KEY=sk-... --app=<app-id>
```

A secret is always readable from the `env` object and from `globalThis.env.get`.
It reaches `process.env` **only when you expose it**, because `process.env` is
readable by any bundled dependency that walks it:

```bash
zeroship secret set OPENAI_API_KEY=sk-... --app=<app-id> --expose
zeroship secret expose OPENAI_API_KEY --app=<app-id>
zeroship secret unexpose OPENAI_API_KEY --app=<app-id>
zeroship secret expose-list --app=<app-id>
```

`zeroship secret list` and `zeroship secret rm NAME` round out the commands. A
key name is 1 to 64 bytes, starts with an uppercase ASCII letter, and continues
with uppercase letters, digits, or underscores.

### Reading a value

The `env` named export, the second argument of `fetch(request, env, ctx)`, and
`globalThis.env.get(name)` all describe the same environment:

```ts
import { env } from "zeroship";

env.PUBLIC_URL;                       // a scalar
globalThis.env.get("OPENAI_API_KEY"); // the string, or null when unset
```

- **`env` is frozen.** You cannot add or replace a member.
- **Namespaces live on the same object.** `env.auth`, `env.db`, `env.kv`,
  `env.storage` and `env.workflows` are the platform's capability handles. If a
  namespace and a scalar share a name, the namespace wins.
- **A secret wins over a variable of the same name** on `env` and on
  `globalThis.env.get`.
- **`process.env` carries variables plus only the secrets you exposed.** A
  variable and an exposed secret of the same name collide there in the
  variable's favor - the opposite of `env`. `globalThis.__env__` is the same
  object as `process.env`, for the npm packages that read through it.

### Names the platform provides

Every app sees one identifier of its own in `process.env`, and a deployed app
with a deploy hash sees a second:

| Name | Present | Value |
| --- | --- | --- |
| `APP_ID` | every app | the app's id |
| `ZEROSHIP_DEPLOY_ID` | when the deploy has one | the deploy's identifier |

Both identify the app to itself, so neither discloses anything. They are on
`process.env`, not on the `env` object. A production build additionally
substitutes `process.env.NODE_ENV` with `"production"` in your bundled server
code at build time.

## Local development

Local development supplies app values the same way a deployment does, through
the `env` object, but from a different source.

### `.env` and `ZS_VAR_`

The dev server loads `<project-root>/.env` and passes it to the runtime. A
value the app should read through `env` takes the `ZS_VAR_` prefix, which is
stripped on the way in:

```dotenv
ZS_VAR_PUBLIC_URL=http://localhost:5173
```

That makes `env.PUBLIC_URL` equal `"http://localhost:5173"`. Only
`ZS_VAR_`-prefixed names cross into `env`; every other variable stays in
`process.env` only. `zeroship serve` applies the same rule to its own process
environment, and the shell environment wins over `.env` on a shared name.

**Local `process.env` is not the deployed one.** The dev server forwards the
launching environment to the runtime, so a read that works locally may be
reading a host value that does not exist in a deployment. Deliver a value
through `ZS_VAR_`, or set it as an app variable or secret, when your code needs
it on both tiers.

`DATABASE_URL` selects the local database: the shell first, then `.env`, then a
SQLite file under `.zeroship/`. It configures that database; it is not a value
to read from app code.

### Local knobs

These are optional, local-only, and read by the dev runtime rather than by the
platform:

| Variable | Effect |
| --- | --- |
| `ZEROSHIP_HEAP_LIMIT_MB` | heap cap for the local runtime |
| `ZEROSHIP_STORAGE_URL` | selects the `env.storage` backend; local files by default |
| `ZEROSHIP_KV_PATH` | the local key-value file |
| `ZEROSHIP_KV_CONFIG_FILE` | a file selecting a non-default local key-value backend |
| `ZEROSHIP_DEV_PORT` | the dev runtime's port; the plugin option wins over it |

The `zeroship` CLI also reads `ZEROSHIP_CONTROL_URL`, `ZEROSHIP_TOKEN`,
`ZEROSHIP_CONFIG` and `ZEROSHIP_CONFIG_HOME`, which address the CLI to a control
plane and a project file. Those are tooling configuration, not values app code
reads; see [`zeroship.jsonc`](project-config.md).
