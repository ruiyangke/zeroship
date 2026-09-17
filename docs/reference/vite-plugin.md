# `@zeroship/vite-plugin`

Reference for the public plugin exported from [`packages/vite-plugin/src/index.ts`](../../packages/vite-plugin/src/index.ts).

## What it owns

- Node-compat shims and `zeroship` module resolution
- Server-procedure discovery for the SSR bundle
- The synthetic server entry `virtual:zeroship/_server-entry`
- The zeroship dev runtime and production `.zship` build

The `zeroship` import stays external in the built server artifact. The runtime
provides its native exports. In dev, the environment preserves that reserved
specifier and ModuleRunner imports the same runtime module through its evaluator.
The plugin does not emit JavaScript implementations of these helpers or resolve
them to the Node test stub.

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
| `config` | none | Escape hatch: a partial config object, or `(resolved) => partial` applied after the file loads and after environment selection. It may not change `app`, `control`, `runtime_date`, `build.output`, `migrations.dir` or `migrations.out`; attempting to is an error naming the field. |

With no `zeroship.jsonc` anywhere, the plugin runs on the schema defaults
(`build.mode: "full"`, `build.dist: "dist"`, `build.output: "dist/app.zship"`,
`migrations.dir: "migrations"`, `migrations.out: "generated/zeroship"`), which
is what keeps `zeroship()` working in a scratch directory.

### `devAuth`

The `pnpm dev` implementation of the platform auth contract - the peer of
`env.db` to SQLite and `env.kv` to redb. When enabled, Vite middleware serves
the same-origin `/__zeroship/auth/*` endpoints the `@zeroship/auth` client
drives and supplies a logged-in identity to `env.auth.getUser()` server-side,
with no gateway, no external auth service and no control plane.

| Value | Effect |
| --- | --- |
| `true` (the dev default) | One built-in dev user (`pws_dev...`, `dev@localhost`, scopes `openid profile email`). |
| `{ id?, email?, ... }` or `{ user: {...} }` | One configured dev user. |
| `{ users: [...], defaultUserId? }` | Several users; `/authorize` renders a dev picker so you can switch identity or scope set. |
| `false` | Disabled. `/__zeroship/auth/*` falls through to the user module and `env.auth.getUser()` returns `null`. |

A configured user is `{ id?, email?, name?, avatar?, scopes? }`. **There is no
`password` field.** The dev login form prefills and validates the deterministic
password produced by `devPasswordFor` in `packages/vite-plugin/src/dev-auth.ts`.
It is a local test credential rather than a secret, and the deployed platform's
signup policy refuses it.

The provider lives in Vite's development middleware and is structurally absent
from any production `.zship`.

## Migration-first type generation (`gen-types`)

The plugin records the migrations under `migrations.dir` in-process through its
pure-JS recorder, then passes the resulting IR envelopes to `zeroship-migrate-node`'s
`genArtifacts` renderer. It writes into `migrations.out` (both keys come from
[`zeroship.jsonc`](project-config.md), defaults `migrations` and
`generated/zeroship`): `env.db.ts` (a generated `@zeroship/db` `t.*()` schema
module), `schema.runtime.json` (the `RuntimeSchemaDescriptor`), and
`migrations.ir.json` (the recorded migration set `zeroship migrate` posts).
Commit that directory. There is no CLI subprocess, and `.zship` packing does not
carry or read migration documents.

When it runs:

- **Dev** — the plugin regenerates the artifacts **on dev-server boot** (`configureServer`, so a migration changed while the server was down is picked up immediately) and **on any change under the migrations dir** (`hotUpdate`). It is fire-and-forget: a malformed migration **logs** an error and never crashes the dev server. The migrations dir is added to the Vite watcher so changes are observed even though app code does not import the `.ts` sources.
- **Build** — `buildStart` runs gen-types once. In a **production** build it runs `--check` (a generated-artifact check: `env.db.ts` or `schema.runtime.json` that no longer tracks the migrations fails the build, exit non-zero). A non-production `vite build --mode development` **regenerates** (writes) instead.

The gen-types orchestrator links the N-API renderer in-process. A renderer load,
recording, or fold error is logged in dev without taking down the dev server. In
production, the generated-artifact check is a hard build error; there is no
missing-CLI no-op path.

### Type activation

The generated `env.db.ts` is the canonical `Env.db` augmentation. Apps include it in `tsconfig.json`:

```json
{
  "include": ["src", "generated/zeroship/env.db.ts"]
}
```

That path is `<migrations.out>/env.db.ts`; if the project moves `migrations.out`,
the `include` moves with it.

Do not also add `@zeroship/db/env` or a `zeroship-schema` path alias. That declared-schema alias path is retired; the generated file is the single source of strong `env.db` typing.

## Procedure discovery in the active build path

Today, a procedure is recorded for the manifest only when all of the following are true:

1. The file has a top-level `"use server"` directive.
2. The exported binding is initialized by a recognized wrapper call.
3. The wrapper is a named import from `@zeroship/rpc/server`.

The static matcher recognizes the full set of `@zeroship/rpc/server` wrappers as discovery markers — see [docs/reference/rpc.md](./rpc.md) for the canonical list and signatures.

Namespace imports and default imports are ignored by the static matcher. Plain exports stay private to the server bundle.
Client stubs for `subscription` preserve `{ kind: "subscription" }` metadata, but the generic `@zeroship/rpc/client` surface does not expose a public subscription API yet; invoking one fails with `UNIMPLEMENTED` instead of falling back to the stream transport.

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

In that file, `listTodos` and `addTodo` become RPC procedures. `helper()` does not.

The old `src/server.{ts,tsx,js,jsx}` and `src/server/**` path convention is no longer sufficient by itself. Legacy paths without the directive only trigger a migration warning.

## Generated client stubs

Client modules can import server procedure exports directly. The transform
replaces those imports with callable procedure references backed by
`@zeroship/rpc/client`:

```ts
import { listTodos, addTodo } from "./index";

const todos = await listTodos({ userId });
await addTodo({ userId, title: "Ship docs" });
```

The generated stubs do not own transport behavior. They delegate to the shared
RPC runtime, so `configureRpcClient({ baseUrl, auth, headers, timeout, retry,
transformer })` affects generated stubs and manual `createRpcClient()` calls in
the same way. This is why app code does not need per-procedure
`clientProcedure(...)` wrappers.

## Config, kind, and `wireId`

- The wrapper's second argument and the legacy `fn.config = { ... }` assignment are both read.
- `fn.config.id` wins; otherwise the default `wireId` is the bare export name.
- Production manifest emission rejects procedures that still rely on the default name. Add an explicit `id` before deploy.
- Duplicate `wireId`s fail the build.

Kind resolution is:

- explicit wrapper kind for `query`, `mutation`, `action`, `stream`, `subscription`
- async generators => `stream`
- generic unary `procedure()` => `mutation`

Names are never used to infer `query`. Reads opt in via `query(...)` or an explicit `config.kind = "query"` so cache and retry policy do not depend on identifier spelling.

`lazy: true` is recorded from either the wrapper config or `fn.config`.
When the entry generator receives explicit server bindings, it emits a
`{ load: () => Promise<Procedure> }` record for a lazy binding. The loader
returns the actual exported procedure, including its validation and kind
metadata. Non-literal `lazy` values warn and stay eager. A module already
statically imported by the app still evaluates during startup.

## Synthetic server entry

[`packages/vite-plugin/src/rpc-registry.ts`](../../packages/vite-plugin/src/rpc-registry.ts)
emits `virtual:zeroship/_server-entry`. It normalizes the app's exports for
native runtime dispatch:

- Schema preparation uses the host's generated `schema.runtime.json` descriptor.
- `default.fetch` retains the user's handler and original default-object
  receiver. When absent, a top-level `fetch` export is used. When neither is
  present, the runtime handles the missing handler.
- `default.rpc` is a dictionary of procedure references or lazy load records,
  keyed by string wire IDs. Declared own string properties are copied first;
  named exports or explicit bindings take precedence. Names such as
  `__proto__` and `constructor` are ordinary own keys. A callable or array
  `default.rpc` is rejected.

The generated entry supplies callable references and module imports. The
[native dispatcher](../../crates/zeroship-runtime/src/rpc/dispatch/mod.rs)
owns HTTP RPC invocation, validation, capability context and response framing.
See [the deploy contract](zeroship-standard.md).

## See also

- [`vite-environment-api.md`](vite-environment-api.md)
- [`rpc.md`](rpc.md)
- [`zeroship-standard.md`](zeroship-standard.md)
