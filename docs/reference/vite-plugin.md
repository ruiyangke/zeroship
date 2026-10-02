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
| `app` | the sole declared app | Which of the workspace's `apps` this build is, by LOCAL LABEL. The app decides which databases are folded, packed and served, so a workspace declaring several must say which; one declaring one implies it. |
| `configPath` | auto-discovery | Path to `zeroship.jsonc`, absolute or relative to the Vite root. An explicit path takes precedence over `ZEROSHIP_CONFIG` and app-root auto-discovery. A path that does not exist throws. |
| `env` | none | Selects a named entry from the file's `environments` block - the plugin's equivalent of the CLI's `--env=`. There is no implicit environment and no `ZEROSHIP_ENV`. |
| `config` | none | Escape hatch: a partial config object, or `(resolved) => partial` applied after the file loads and after environment selection. It may not change `name`, `control`, `runtime_date`, `build.output`, any member of a `databases` or `apps` entry, `secrets` or an environment's `protected`; attempting to fails the build naming the field. |

With no `zeroship.jsonc` anywhere, the plugin runs on the schema defaults
(`build.mode: "full"`, `build.dist: "dist"`, `build.output: "dist/app.zship"`),
which is what keeps `zeroship()` working in a scratch directory. A database's
migration sources and its fold have NO default: two databases sharing one
directory would be one schema standing in for another, so the file is their one
holder and a scratch directory simply declares no database.

`devServerPort` takes precedence over the `ZEROSHIP_DEV_PORT` environment
variable, which in turn takes precedence over the `3001` default. The runtime
listens on that port; Vite serves your app on its own port. The two are
independent, so `vite --port` does not move the runtime. If another app already
holds the runtime port, server calls fail with an `RpcError` whose `code` is
`UNAVAILABLE` (see [rpc.md](rpc.md#errors-and-retries)), and the dev server
prints a banner naming the clash and the `devServerPort` fix.

The dev server runs the `zeroship` binary named by `ZEROSHIP_BIN`, else the
one installed in your project, else `zeroship` on your `PATH`. If that binary
cannot be run at all - a stale `ZEROSHIP_BIN`, a path that is not an
executable file, or no CLI installed - the dev server prints
`Failed to start API server`, naming the binary and which of those three chose
it, and keeps serving your page. Server calls then fail with `code`
`UNAVAILABLE`, `retryable: false` and `details.state: "unstartable"`, and are
never forwarded to whatever else holds the runtime port. Nothing is retried on
its own: fix the binary, then save a change to your app or restart `pnpm dev`.

Whenever the dev server answers for the runtime instead of forwarding to it,
the error carries `code: "UNAVAILABLE"` and a `details.state` saying why. The
restart budget below is `MAX_RAPID_RESTARTS` in the plugin's `constants.ts`,
and a runtime counts as having started once it stays up `RUNTIME_HEALTHY_MS`.

| `details.state` | `retryable` | Meaning | `details.attempts` |
| --- | --- | --- | --- |
| `failing` | `true` | The runtime exited before it started and a restart is scheduled, or it is being replaced. | Consecutive exits before starting. |
| `fatal` | `false` | The runtime exited before it started more times in a row than the restart budget allows, and was given up on. The terminal banner quotes what it said. Fix the cause, then restart `pnpm dev`. | Consecutive exits before starting. |
| `unstartable` | `false` | The runtime binary could not be run at all (above). | Consecutive exits before starting that preceded it, if any. |
| `unsettled` | `false` | The source kept changing while each runtime loaded your entry, so each was replaced, more times in a row than the restart budget allows. The message names what changed after the last one began loading. Stop whatever keeps rewriting it, then save a change to your app or restart `pnpm dev`. | Runtimes replaced in a row. |

A runtime that fails while loading your entry is not given up on. When your
source changed after that load began, the dev server replaces the runtime at
once; when it did not, the runtime keeps answering with its error until you
save a change. Only a run of replacements with no runtime staying up
`RUNTIME_HEALTHY_MS` in between, and no app change, counts toward `unsettled`.

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

Your committed migrations are the schema source of truth. The build folds each
database's migrations into generated artifacts under that database's own `out`:

- `env.db.ts` - this database's typed surface, as a generated `@zeroship/db`
  schema module. It declares the database under its LABEL on `EnvDatabases`,
  and declares `Env.db` as that entry when the app names it `primary`.
- `schema.runtime.json` - the runtime schema descriptor carried into the
  `.zship`.

Commit that directory. Those two files are the whole of it: the migrations
themselves are never rewritten into a generated file. The `.zship` does not
carry migration documents either - it carries only the generated descriptor, and
`zeroship migrate` records your `migrations/*.ts` when you run it (see [the CLI
reference](cli.md), [the project config](project-config.md) and [the deploy
contract](zeroship-standard.md)).

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

The generated `env.db.ts` is the canonical augmentation. Apps include it in
`tsconfig.json`:

```json
{
  "include": ["src", "generated/zeroship/env.db.ts"]
}
```

That path is `<out>/env.db.ts` for the database that declared it; if the entry
moves its `out`, the `include` moves with it. **Include one per database the
app uses**: each module augments `EnvDatabases` under its own label, so a
database left out of the `include` is a label `env.databases` does not have.
Exactly one of them - the primary's - also declares `Env.db`, which is why a
database may not be one app's primary and another's secondary.

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

## Building the app

`vite build` builds the app through Vite's app builder, which `zeroship()`
opts the build into. It builds the client environment, then the worker in the
`zeroship` environment, then packs the `.zship`.

- The worker is compiled from your Vite config. Your plugins, your top-level
  `define` and your `resolve.alias` apply to server code, as they do in
  `pnpm dev`. A plugin that must stay off server code, such as one that writes
  files from `writeBundle`, scopes itself with Vite's `applyToEnvironment`.
- The plugin owns the worker's shape: one minified ES module,
  `server/index.js` under `build.dist`, with no public files and no build
  manifest. `NODE_ENV`, read as `process.env.NODE_ENV`,
  `global.process.env.NODE_ENV` or `globalThis.process.env.NODE_ENV`, is
  replaced with `"production"` whatever the shell sets; every other
  `process.env` reference, in any of those spellings, stays live for the
  runtime to answer.
- Plugin names that begin with `zeroship:` are reserved for this package.
  `pnpm dev` builds the worker for its local deployment from your Vite config
  too, and in that build it replaces every plugin carrying one of the
  package's names with a fresh instance, so an app plugin must not use the
  prefix.
- A tool that calls `vite.build()` with the app's config, such as a test
  harness or a deploy script, is refused before the client writes anything:
  `vite.build()` builds a single environment, so it would pack no worker.
  Call `createBuilder(config, null)` and then `buildApp()` instead, which is
  what `vite build` does. `vite build --watch` is refused too.

## See also

- [`vite-environment-api.md`](vite-environment-api.md)
- [`project-config.md`](project-config.md)
- [`rpc.md`](rpc.md)
- [`db.md`](db.md)
- [`auth.md`](auth.md)
- [`zeroship-standard.md`](zeroship-standard.md)
- [`zship.md`](zship.md)