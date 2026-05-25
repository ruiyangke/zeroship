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

## Exposed but not currently effectful

These fields exist on `ZeroshipOptions`, but the current `zeroship()` pipeline does not use them to change emitted behavior:

| Option | Current reality |
| --- | --- |
| `rpcEndpoint` | The transform receives it, but generated client stubs still call `/_zs/v1/<wireId>`. Dev middleware also accepts legacy `/_rpc` and `/rpc` routes for migration parity. |
| `rpc.strict` | `resolveRpcStrict()` and `server-graph.ts` exist, but the build path in [`sdks/vite-plugin/src/build.ts`](../../sdks/vite-plugin/src/build.ts) still emits manifest metadata from wrapper discovery only. |

## Procedure discovery in the active build path

Today, a procedure is recorded for the manifest only when all of the following are true:

1. The file has a top-level `"use server"` directive.
2. The exported binding is initialized by a recognized wrapper call.
3. The wrapper is a named import from `@zeroship/server` or `@zeroship/rpc`.

Recognized wrappers: `procedure`, `query`, `mutation`, `action`, `stream`, `subscription`.

Namespace imports and default imports are ignored by the static matcher. Plain exports stay private to the server bundle.

```ts
"use server";

import { query, mutation } from "@zeroship/server";

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

## Config, kind, and `wireId`

- The wrapper's second argument and the legacy `fn.config = { ... }` assignment are both read.
- `fn.config.id` wins; otherwise the default `wireId` is the bare export name.
- Production manifest emission rejects procedures that still rely on the default name. Add an explicit `id` before deploy.
- Duplicate `wireId`s fail the build.

Kind resolution is:

- explicit wrapper kind for `query`, `mutation`, `action`, `stream`, `subscription`
- name-based inference for `procedure()`:
  `get*`, `list*`, `find*`, `search*`, `count*`, `read*`, `fetch*` => `query`
- async generators => `stream`
- everything else => `mutation`

`lazy: true` is supported in either the wrapper config or `fn.config`. When present, the synthetic server entry emits a dynamic `import()` wrapper instead of an eager namespace import. Non-literal `lazy` values warn and stay eager.

## Synthetic server entry

[`sdks/vite-plugin/src/rpc-registry.ts`](../../sdks/vite-plugin/src/rpc-registry.ts) emits `virtual:zeroship/_server-entry`. Its job is normalization, not dispatch:

- `default.schema` is passed through
- `default.fetch` is passed through
- `default.rpc` is a plain object keyed by `wireId`

The runtime-side dispatcher lives in [`sdks/bootstrap/README.md`](../../sdks/bootstrap/README.md) and [`zs-standard.md`](zs-standard.md), not in the generated entry.

## See also

- [`vite-environment-api.md`](vite-environment-api.md)
- [`zs-standard.md`](zs-standard.md)
- [`sdks/bootstrap/README.md`](../../sdks/bootstrap/README.md)
