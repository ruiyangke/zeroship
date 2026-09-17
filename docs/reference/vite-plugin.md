# `@zeroship/vite-plugin`

`zeroship()` is the build plugin a zeroship app installs in `vite.config.ts`. It
turns server modules into RPC, runs server code in a real runtime during
development, folds your committed migrations into typed `env.db`, and packs the
deployable `.zship`. This page covers what you configure and what the build does
with it.

## What it owns

One plugin installed once carries the whole server half of your app:

- **Discovery.** A module whose first statement is `"use server"` is a server
  module, and its wrapped exports become RPC procedures.
- **Client rewriting.** Importing a wrapped procedure from client code is
  rewritten into a typed RPC call, so you call server exports like local
  functions.
- **The runtime module.** `import { env } from "zeroship"` resolves to the
  runtime environment in dev and in the deployed artifact. You never configure
  this import; both paths resolve it to the same runtime.
- **The dev runtime.** `pnpm dev` starts a zeroship runtime beside Vite and
  forwards your server routes to it.
- **Schema typing.** Your committed migrations are folded into the generated
  `env.db` types (see below).
- **The artifact.** `pnpm build` emits the `.zship` that `zeroship deploy`
  uploads.

## Usage

```ts
import { defineConfig } from "vite";
import { zeroship } from "@zeroship/vite-plugin";

export default defineConfig({
  plugins: [zeroship()],
});
```

## Options

The option bag is small on purpose. The build shape - `mode`, the server entry,
the dist dir, the `.zship` path, and where migrations are authored and folded -
lives in [`zeroship.jsonc`](project-config.md), because the `zeroship` CLI needs
those same facts and cannot read `vite.config.ts`. What is left is what varies
per developer machine, plus the three levers that point at the file.

| Option | Default | Behavior |
| --- | --- | --- |
| `devServerPort` | `3001` | Port for the zeroship dev runtime. |
| `devAuth` | `true` in dev | Dev-tier auth. See below. |
| `configPath` | auto-discovery | Path to `zeroship.jsonc`, absolute or relative to the Vite root. An explicit path takes precedence over `ZEROSHIP_CONFIG` and app-root auto-discovery. A path that does not exist throws. |
| `env` | none | Selects a named entry from the file's `environments` block - the plugin's equivalent of the CLI's `--env=`. There is no implicit environment and no `ZEROSHIP_ENV`. |
| `config` | none | Escape hatch: a partial config object, or `(resolved) => partial` applied after the file loads and after environment selection. It may not change `name`, `app`, `control`, `runtime_date`, `build.output`, `migrations.dir`, `migrations.out`, `secrets` or an environment's `protected`; attempting to fails the build naming the field. |

With no `zeroship.jsonc` anywhere, the plugin runs on the schema defaults
(`build.mode: "full"`, `build.dist: "dist"`, `build.output: "dist/app.zship"`,
`migrations.dir: "migrations"`, `migrations.out: "generated/zeroship"`), which
is what keeps `zeroship()` working in a scratch directory.

`devServerPort` takes precedence over the `ZEROSHIP_DEV_PORT` environment
variable, which in turn takes precedence over the `3001` default. The runtime
listens on that port; Vite serves your app on its own port. The two are
independent, so `vite --port` does not move the runtime. If another app already
holds the runtime port, server calls fail with an `RpcError` whose `code` is
`UNAVAILABLE` (see [rpc.md](rpc.md#errors-and-retries)), and the dev server
prints a banner naming the clash and the `devServerPort` fix.

### `devAuth`

The `pnpm dev` implementation of the [platform auth contract](auth.md):
`env.db`, `env.kv` and the auth session are all served locally, with no gateway,
no external auth service and no control plane. When enabled, the dev server
serves the same-origin `/__zeroship/auth/*` endpoints the `@zeroship/auth`
client drives and supplies a logged-in identity to `env.auth.getUser()`
server-side.

| Value | Effect |
| --- | --- |
| `true` (the dev default) | One built-in dev user (`pws_dev...`, `dev@localhost`, scopes `openid profile email`). |
| `{ id?, email?, ... }` or `{ user: {...} }` | One configured dev user. |
| `{ users: [...], defaultUserId? }` | Several users; the dev sign-in form renders a picker so you can switch identity or scope set. |
| `false` | Disabled. `/__zeroship/auth/*` falls through to your own routes and `env.auth.getUser()` returns `null`. |

A configured user is `{ id?, email?, name?, avatar?, scopes? }`. **There is no
`password` field.** The dev sign-in form prefills and validates a password
derived from the user id; it is visible on the form, is a local test credential
rather than a secret, and the deployed platform's signup policy refuses it. A
custom `id` must be `pws_` followed by exactly 20 lowercase base36 characters -
the only subject shape the deployed gateway accepts - so omit `id` unless you
need a specific one.

The dev auth provider is dev-only: it is not part of any production `.zship`.

## Migration-first type generation (`gen-types`)

Your committed migrations are the schema source of truth. The build folds them
into generated artifacts under `migrations.out` (default `generated/zeroship`):

- `env.db.ts` - the typed `env.db` surface, as a generated `@zeroship/db`
  schema module.
- `schema.runtime.json` - the runtime schema descriptor carried into the
  `.zship`.
- `migrations.ir.json` - the recorded migration set that `zeroship migrate`
  posts.

Commit that directory. The `.zship` does not carry migration documents:
`zeroship migrate` posts the recorded set, and the artifact carries only the
generated descriptor (see [the project config](project-config.md) and
[the deploy contract](zeroship-standard.md)).

When it runs:

- **Dev** - the artifacts are regenerated when the dev server boots (so a
  migration changed while the server was down is picked up immediately) and on
  any change under the migrations directory. Regeneration is fire-and-forget: a
  malformed migration logs an error to the dev server console and never crashes
  the dev server. The migrations directory is watched so edits are seen even
  though app code does not import the `.ts` sources.
- **Build** - generation runs once at build start. A **production** build runs
  a generated-artifact check: if `env.db.ts` or `schema.runtime.json` has
  drifted from the migrations, the build fails, non-zero. A non-production
  `vite build --mode development` **regenerates** (writes) instead, which is how
  you refresh the committed types locally.
- **`pnpm migrate`** - regenerates the artifacts and then applies the migrations
  to the dev database, in one command. It is a separate, explicit step: `pnpm
  dev` reports the dev schema state but never applies migrations.

A malformed migration is a fault in your source, so it is logged to the dev
server console and survived. A platform fault - an unwritable output directory,
say - is not one you can fix in a migration, so it stops the dev server at boot
instead of leaving `env.db` stale. The production check is a hard build error
rather than a build that ships a drifted artifact.

### Type activation

The generated `env.db.ts` is the canonical `Env.db` augmentation. Apps include
it in `tsconfig.json`:

```json
{
  "include": ["src", "generated/zeroship/env.db.ts"]
}
```

That path is `<migrations.out>/env.db.ts`; if the project moves `migrations.out`,
the `include` moves with it.

Do not also add a `@zeroship/db/env` or a `zeroship-schema` path alias. The
generated file is the single source of strong `env.db` typing.

## Procedure discovery in the active build path

A procedure is published as an RPC endpoint only when all of the following are
true:

1. The file has a top-level `"use server"` directive.
2. The exported binding is initialized by a recognized wrapper call.
3. The wrapper is a named import from `@zeroship/rpc/server`.

The recognized wrappers are the full `@zeroship/rpc/server` set - see
[`rpc.md`](./rpc.md) for the canonical list and signatures.

Namespace imports (`import * as rpc`) and default imports are ignored. Plain
exports stay private to the server bundle: other server code can call them, but
they are not network-reachable. A path such as `src/server/` does not opt a file
in by itself; without the directive the build warns that the file will not be
published as an RPC endpoint.

Client stubs for `subscription` preserve `{ kind: "subscription" }` metadata, but
the generic `@zeroship/rpc/client` surface does not expose a public subscription
API yet; invoking one fails with an `RpcError` whose `code` is `UNIMPLEMENTED`
(see [rpc.md](rpc.md#errors-and-retries)) instead of falling back to the stream
transport.

```ts
"use server";

import { query, mutation } from "@zeroship/rpc/server";

export const listTodos = query(async () => []);
export const addTodo = mutation(async (input) => input, {
  id: "todos.add",
});

export async function helper() {
  return [];
}
```

In that file, `listTodos` and `addTodo` become RPC procedures. `helper()` does
not.

## Generated client stubs

Client modules can import server procedure exports directly. The build replaces
those imports with callable procedure references created by
`@zeroship/rpc/client`:

```ts
import { listTodos, addTodo } from "./index";

const todos = await listTodos({ userId });
await addTodo({ userId, title: "Ship docs" });
```

The generated stubs do not own transport behavior. They delegate to the shared
RPC runtime, so `configureRpcClient({ ... })` - `baseUrl`, `auth`, `headers`,
`timeout`, `retry`, `transformer` - affects generated stubs and manual
`createRpcClient()` calls in the same way (see [rpc.md](rpc.md#vite-generated-calls)
for what each option does). This is why app code does not need per-procedure
client wrappers.

## Config, kind, and `wireId`

A procedure's config may be given as the wrapper's second argument, as a
`<fn>.config = { ... }` assignment, or both; the assignment wins where they
overlap.

- `fn.config.id` wins; otherwise the default `wireId` is the bare export name.
- A production build rejects procedures that still rely on the default name.
  Add an explicit `id` before deploy. An `id` is any non-empty string; it is
  published verbatim as the segment after `/__zeroship/v1/`, so the documented
  convention is a dotted `namespace.verb` such as `todos.list` (see
  [rpc.md](rpc.md)).
- Duplicate `wireId`s fail the build.

Kind resolution is:

- explicit wrapper kind for `query`, `mutation`, `action`, `stream`,
  `subscription`
- async generators => `stream`
- generic unary `procedure()` => `mutation`

Names never imply `query`. Reads opt in via `query(...)` or an explicit
`config.kind = "query"` so cache and retry policy do not depend on identifier
spelling.

`lazy: true` is recorded from either the wrapper config or `fn.config`. A lazy
procedure is loaded on first call instead of at startup. Non-literal `lazy`
values warn and stay eager. A module the app already statically imports still
evaluates during startup.

## Synthetic server entry

Your exports reach the runtime through your module's default export, which must
satisfy [the deploy contract](zeroship-standard.md):

- Schema preparation uses the generated `schema.runtime.json` descriptor from
  your migrations.
- `default.fetch` is your request handler, called with its original `this`.
  When absent, a top-level `fetch` export is used. When neither is present, the
  runtime handles the missing handler.
- `default.rpc` is a dictionary of procedures keyed by wire ID. Own string keys
  are kept, and the procedures the build discovered from your `"use server"`
  modules take precedence over keys you declared by hand. A callable or array
  `default.rpc` is rejected.

## See also

- [`vite-environment-api.md`](vite-environment-api.md)
- [`project-config.md`](project-config.md)
- [`rpc.md`](rpc.md)
- [`db.md`](db.md)
- [`auth.md`](auth.md)
- [`zeroship-standard.md`](zeroship-standard.md)
- [`zship.md`](zship.md)