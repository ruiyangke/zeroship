# `@zeroship/vite-plugin`

Reference for the public plugin exported from [`sdks/vite-plugin/src/index.ts`](../../sdks/vite-plugin/src/index.ts).

## What it owns

- Node-compat shims and `zeroship` module resolution
- Server-procedure discovery for the SSR bundle
- The synthetic server entry `virtual:zeroship/_server-entry`
- The zeroship dev runtime and production `.zship` build

## Usage

```ts
import { defineConfig } from "vite";
import { zeroship } from "@zeroship/vite-plugin";

export default defineConfig({
  plugins: [zeroship({ mode: "full" })],
});
```

## Live options

| Option | Default | Current behavior |
| --- | --- | --- |
| `serverEntry` | auto-detected | Overrides server-entry discovery for dev and build. |
| `devServerPort` | `3001` | Port for the zeroship dev runtime. |
| `mode` | `"full"` | `"full"` builds client + SSR worker; `"static"` skips the SSR sub-build and omits `manifest.worker`. |
| `migrations.dir` | `"migrations"` | The op.* migration source dir. The gen-types step reads it to fold migrations into the generated `env.db` artifacts; `.zship` packing does not carry or read migration documents. |
| `migrations.genTypesOut` | `"generated/zeroship"` | Where the gen-types step writes `env.db.ts` + `schema.runtime.json`. Commit this dir and include `generated/zeroship/env.db.ts` in the app tsconfig. |
| `migrations.cliPath` | resolved (`ZEROSHIP_MIGRATE_JS_BIN` → `node_modules/.bin`) | Explicit path to the `zeroship-migrate-js` CLI. |

## Migration-first type generation (`gen-types`)

The plugin shells the **existing** `zeroship-migrate-js gen-types --dir <migrations> --out <outDir> [--check]` subcommand to record `migrations/*.ts` through the sandboxed recorder, fold the transient IR in memory, and emit the typed `env.db` surface — two artifacts, `env.db.ts` (a generated `@zeroship/db` `t.*()` schema module) and `schema.runtime.json` (the `RuntimeSchemaDescriptor`). The plugin never re-implements type generation; it is a thin client of the CLI.

When it runs:

- **Dev** — the plugin regenerates the artifacts **on dev-server boot** (`configureServer`, so a migration changed while the server was down is picked up immediately) and **on any change under the migrations dir** (`hotUpdate`). It is fire-and-forget: a malformed migration **logs** an error and never crashes the dev server. The migrations dir is added to the Vite watcher so changes are observed even though app code does not import the `.ts` sources.
- **Build** — `buildStart` runs gen-types once. In a **production** build it runs `--check` (a generated-artifact check: `env.db.ts` or `schema.runtime.json` that no longer tracks the migrations fails the build, exit non-zero). A non-production `vite build --mode development` **regenerates** (writes) instead.

Binary resolution mirrors the dev-runtime convention: an explicit `migrations.cliPath`, else `ZEROSHIP_MIGRATE_JS_BIN`, else `<root>/node_modules/.bin/zeroship-migrate-js`. If the binary is **absent in dev**, the step warns once and no-ops — the generated `env.db.ts` stays valid. For the production `--check` gate it then falls through to the bare `zeroship-migrate-js` **PATH** name, so a `cargo install`-style binary on `$PATH` resolves; a genuinely-missing binary surfaces a real spawn `ENOENT` and hard-fails the gate (a misconfigured CI must not silently pass).

### Type activation

The generated `env.db.ts` is the canonical `Env.db` augmentation. Apps include it in `tsconfig.json`:

```json
{
  "include": ["src", "generated/zeroship/env.db.ts"]
}
```

Do not also add `@zeroship/db/env` or a `zeroship-schema` path alias. That declared-schema alias path is retired; the generated file is the single source of strong `env.db` typing.

## Exposed but not currently effectful

These fields exist on `ZeroshipOptions`, but the current `zeroship()` pipeline does not use them to change emitted behavior:

| Option | Current reality |
| --- | --- |
| `rpcEndpoint` | The transform receives it, but generated client stubs and the shared RPC client use the shipped `/__zeroship/v1/<wireId>` path. |
| `rpc.strict` | `resolveRpcStrict()` and `server-graph.ts` exist, but the build path in [`sdks/vite-plugin/src/build.ts`](../../sdks/vite-plugin/src/build.ts) still emits manifest metadata from wrapper discovery only. |

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

`lazy: true` is supported in either the wrapper config or `fn.config`. When present, the synthetic server entry emits a dynamic `import()` wrapper instead of an eager namespace import. Non-literal `lazy` values warn and stay eager.

## Synthetic server entry

[`sdks/vite-plugin/src/rpc-registry.ts`](../../sdks/vite-plugin/src/rpc-registry.ts) emits `virtual:zeroship/_server-entry`. Its job is to normalize the app module and delegate RPC fall-through to `@zeroship/bootstrap`:

- schema comes from the generated `schema.runtime.json` descriptor, not the synthetic default export
- `default.fetch` is a shared bootstrap fetch handler that routes `/__zeroship/v1/<wireId>` and falls through to the user's own fetch for non-RPC paths
- `default.rpc` is a plain object keyed by `wireId`

The runtime-side dispatcher and stream encoder live in [`sdks/bootstrap/README.md`](../../sdks/bootstrap/README.md) and [`zeroship-standard.md`](zeroship-standard.md), not as generated helper code in the entry.

## See also

- [`vite-environment-api.md`](vite-environment-api.md)
- [`rpc.md`](rpc.md)
- [`zeroship-standard.md`](zeroship-standard.md)
- [`sdks/bootstrap/README.md`](../../sdks/bootstrap/README.md)
