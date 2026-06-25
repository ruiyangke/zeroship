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
| `migrations.dir` | `"migrations"` | The op.* migration dir holding the committed `.ir.json` set. Both the `.zship` packer and the gen-types step read it. |
| `migrations.genTypesOut` | `"generated/zeroship"` | Where the gen-types step writes `env.db.ts` + `schema.runtime.json`. **Committed, but kept OUTSIDE the app tsconfig `include`** — see "Migration-first type generation" below. |
| `migrations.cliPath` | resolved (`ZEROSHIP_MIGRATE_JS_BIN` → `node_modules/.bin`) | Explicit path to the `zeroship-migrate-js` CLI. |

## Migration-first type generation (`gen-types`)

The plugin shells the **existing** `zeroship-migrate-js gen-types --dir <migrations> --out <outDir> [--check]` subcommand to fold the committed `.ir.json` migration set into the typed `env.db` surface — two artifacts, `env.db.ts` (a generated `@zeroship/db` `t.*()` schema module) and `schema.runtime.json` (the `RuntimeSchemaDescriptor`). The plugin never re-implements type generation; it is a thin client of the same CLI it already shells for `record`/`build`.

When it runs:

- **Dev** — on any change under the migrations dir (`hotUpdate`), the plugin regenerates the artifacts. It is fire-and-forget: a malformed migration **logs** an error and never crashes the dev server. The migrations dir is added to the Vite watcher so changes are observed even though app code does not import the `.ts` sources.
- **Build** — `buildStart` runs gen-types once. In a **production** build it runs `--check` (a **drift gate**: a committed artifact that no longer tracks the migrations fails the build, exit non-zero). A non-production `vite build --mode development` **regenerates** (writes) instead.

Binary resolution mirrors the dev-runtime convention: an explicit `migrations.cliPath`, else `ZEROSHIP_MIGRATE_JS_BIN`, else `<root>/node_modules/.bin/zeroship-migrate-js`. If the binary is **absent in dev**, the step warns once and no-ops — the committed `env.db.ts` stays valid. The production `--check` drift gate hard-fails on a missing binary (a misconfigured CI must not silently pass).

### P5 deferral — types are generated + committed, NOT yet activated

P3 wires the **mechanical** generation only. The generated `env.db.ts` and the shipped `@zeroship/db` `env.d.ts` **both** declare `declare module "zeroship" { interface Env { db } }`; having both in one tsc program is a `TS2717` duplicate-property error unless the `Db<>` types are byte-identical. So the artifacts are emitted into a **committed** dir (`generated/zeroship/` by default) that is deliberately **outside the app tsconfig `include`** (not under `src/`, and not `.zeroship/`, which is gitignored). They are generated, committed, and drift-gated, but **not** wired into the typecheck, and the `@zeroship/db`/zeroship-schema alias is untouched.

The type-activation cutover — deleting `export default { schema }` as the source of truth, swapping the alias, and folding `env.db.ts` into the tsc `include` — is **P5**. Until then both the declared schema and the generated types coexist without colliding.

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

- `default.schema` is passed through
- `default.fetch` is a shared bootstrap fetch handler that routes `/__zeroship/v1/<wireId>` and falls through to the user's own fetch for non-RPC paths
- `default.rpc` is a plain object keyed by `wireId`

The runtime-side dispatcher and stream encoder live in [`sdks/bootstrap/README.md`](../../sdks/bootstrap/README.md) and [`zeroship-standard.md`](zeroship-standard.md), not as generated helper code in the entry.

## See also

- [`vite-environment-api.md`](vite-environment-api.md)
- [`rpc.md`](rpc.md)
- [`zeroship-standard.md`](zeroship-standard.md)
- [`sdks/bootstrap/README.md`](../../sdks/bootstrap/README.md)
