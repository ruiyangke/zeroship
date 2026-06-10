# `@zeroship/rpc`

`@zeroship/rpc` is the public RPC package for server procedures and typed
clients. It has three stable subpaths:

- `@zeroship/rpc/server` — server-side procedure wrappers.
- `@zeroship/rpc/client` — manual clients and the runtime used by generated stubs.
- `@zeroship/rpc/types` — type-only contract helpers.

The package is implemented in `sdks/rpc/`. The Vite plugin discovers server
procedures, generates client stubs, and emits manifest resources, but the
transport, retry, timeout, auth, transformer, and error behavior live in this
package so non-Vite TypeScript projects can use the same client manually.

## Server authoring

Server modules opt into discovery with a file-level `"use server"` directive.
Only exports wrapped with a procedure helper are published as RPC endpoints;
plain helper exports stay private.

```ts
"use server";

import { query, mutation, stream } from "@zeroship/rpc/server";
import { env } from "zeroship";

export const listTodos = query(
  async (input: { userId: string }) => {
    return env.db.todos.find({ userId: input.userId });
  },
  { id: "todos.list" },
);

export const createTodo = mutation(
  async (input: { userId: string; title: string }) => {
    return env.db.todos.insert({ ...input, done: false });
  },
  { id: "todos.create", idempotent: true },
);

export const subscribeTodos = stream(
  async function* (input: { userId: string }) {
    const live = env.db.live(() => env.db.todos.find({ userId: input.userId }));
    try {
      for await (const rows of live) yield { kind: "snapshot", rows };
    } finally {
      live.close();
    }
  },
  { id: "todos.subscribe" },
);
```

Production builds require explicit `id` values. Development may use export
names for convenience, but deployable wire IDs must be pinned so refactors do
not change HTTP paths. The active wrapper helpers are:

- `procedure(handler, { kind, id, ... })`
- `query(handler, { id, ... })`
- `mutation(handler, { id, idempotent, ... })`
- `action(handler, { id, ... })`
- `stream(handler, { id, ... })`
- `subscription(handler, { id, ... })`

`subscription` metadata is recognized by discovery and the lower-level
transport has a WebSocket helper, but the public generated/manual client shape
does not expose subscriptions yet. Use `stream(...)` for shipped live feeds.

## Procedure auth (authenticated by default)

When deployed behind the gateway, **every RPC procedure requires an authenticated
end-user by default** — a procedure with no auth policy resolves to `auth: user`,
not public. This is fail-closed by design: forgetting to set auth yields a loud
`401`, never a silent public endpoint. (Per-request identity *inside* a handler is
still read explicitly with `auth.getUser()` / `auth.requireUser()`; the gateway
default just guarantees the caller is authenticated before the handler runs.)

To make a procedure **publicly reachable**, opt in explicitly in the app's
resource policy (`src/server/config.ts`):

```ts
import { defineApp } from "@zeroship/server";

export default defineApp({
  resources: {
    // A whole namespace public: every `wizard.*` procedure resolves to anon.
    "rpc:wizard": { auth: "anon", publiclyAccessible: true },
  },
});
```

`publiclyAccessible: true` is the deliberate confirmation the manifest validator
requires alongside `auth: "anon"` — it makes "this endpoint is intentionally
public" explicit and reviewable. `auth: "admin"` restricts a procedure (or family)
to platform admins. Manifest auth is enforced only by the gateway; the
single-tenant `zeroship serve` and `pnpm dev` runtimes do not gate by policy, so
local runs reach every procedure regardless of its declared auth.

## Vite-generated calls

With `@zeroship/vite-plugin`, client code imports the server export directly:

```ts
import { createTodo, listTodos } from "./index";

const todos = await listTodos({ userId });
const todo = await createTodo(
  { userId, title: "Ship docs" },
  { idempotencyKey: crypto.randomUUID() },
);
```

The transform rewrites those imports to callable procedure references created
by `@zeroship/rpc/client`. Generated stubs and manual clients therefore share
the same behavior: same-origin `/__zeroship/v1/<id>` by default, configurable
`baseUrl`, auth, headers, timeout, retry policy, idempotency, transformer, and
`RpcError` handling.

Install app-level defaults once at boot when generated stubs need anything
beyond same-origin JSON:

```ts
import { configureRpcClient } from "@zeroship/rpc/client";

configureRpcClient({
  baseUrl: "https://my-app.zeroship.ai",
  auth: () => localStorage.getItem("token"),
  timeout: 10_000,
  retry: { attempts: 3 },
});
```

## Manual client

Non-Vite projects should use `createRpcClient` directly:

```ts
import { createRpcClient } from "@zeroship/rpc/client";
import type { InferRpcContract } from "@zeroship/rpc/types";
import type * as server from "./server";

type AppRpc = InferRpcContract<typeof server>;

const rpc = createRpcClient<AppRpc>({
  baseUrl: "https://my-app.zeroship.ai",
  auth: async () => getToken(),
});

const listTodos = rpc.query("todos.list");
const createTodo = rpc.mutation("todos.create", { idempotent: true });

const todos = await listTodos({ userId: "user_..." });
await createTodo({ userId: "user_...", title: "Write docs" });
```

`InferRpcContract<typeof server>` only works when TypeScript can see the server
procedure declarations. It is a type-only source of truth; it does not import
server code into the browser at runtime.

When the server declarations are not available, declare a small contract by
hand:

```ts
import type { Query, Mutation, Stream } from "@zeroship/rpc/types";

type AppRpc = {
  "todos.list": Query<{ userId: string }, Todo[]>;
  "todos.create": Mutation<
    { userId: string; title: string },
    Todo,
    { idempotent: true }
  >;
  "todos.subscribe": Stream<{ userId: string }, TodoSnapshot>;
};
```

If you want a registry-backed escape hatch, pass procedure metadata and call by
ID:

```ts
const procedures = {
  "todos.list": { kind: "query" },
  "todos.create": { kind: "mutation", idempotent: true },
} as const;

const rpc = createRpcClient<AppRpc>({ procedures, baseUrl: "https://..." });
await rpc.call("todos.list", { userId });
```

## Transport

The shipped wire path is `/__zeroship/v1/<wireId>`.

| Kind | Request shape |
| --- | --- |
| `query` | `GET /__zeroship/v1/<id>?input=<base64url-json>` for small inputs. Large query URLs fall back to `POST` with `X-Method: GET`. |
| `mutation` | `POST /__zeroship/v1/<id>` with a JSON body. |
| `action` | Same as mutation, but with action capability on the server side. |
| `stream` | `POST /__zeroship/v1/<id>` with `Accept: text/event-stream`; client consumes AI SDK data stream frames. |

Every request gets `X-Request-Id`. Auth resolvers set
`Authorization: Bearer <token>` when they return a token. Idempotent writes get
an `Idempotency-Key`; retries reuse the same key for the logical call.

## Transformers

The default transformer is `"json"`. It is dependency-free and handles normal
JSON values.

`"superjson"` is opt-in for rich values such as `Date`, `BigInt`, `Map`, `Set`,
`URL`, and typed arrays. It is an optional dependency. If you configure
`transformer: "superjson"`, install `superjson` in the consuming project and
make sure the server manifest uses the same transformer. Plain JSON apps should
leave the default alone.

## Errors and retries

All client failure paths throw `RpcError` from `@zeroship/rpc/client`.
Branch on `error.code`, not on the message:

```ts
import { isRpcError } from "@zeroship/rpc/client";

try {
  await createTodo(input);
} catch (error) {
  if (isRpcError(error) && error.code === "UNAUTHENTICATED") {
    redirectToLogin();
  }
}
```

Retry policy applies to unary calls. Reads can retry when the error is
retryable. Writes retry only when `retryWrites: true` is set or the procedure is
marked `idempotent: true`, because idempotent writes carry a stable
`Idempotency-Key`.

## Streams

For stream procedures, call the generated/manual function and iterate:

```ts
for await (const snapshot of subscribeTodos({ userId })) {
  render(snapshot);
}
```

For AI SDK integrations that want a URL instead of an iterator, stream
procedures expose `streamUrl(input)` on the manual factory result and generated
procedure reference.

## Current boundaries

- There is no public `@zeroship/rpc/react` package. Use TanStack Query or your
  framework's data layer on top of the plain async functions.
- `subscription` is not part of the public client surface yet.
- `Response` objects and arbitrary binary/file bodies are not unary JSON RPC
  results. Use stream procedures, `streamUrl()`, or a normal fetch route for
  raw responses.
- Type inference gives TypeScript types only. Runtime dispatch still needs
  generated metadata from the Vite plugin or explicit metadata supplied to
  `createRpcClient`.
