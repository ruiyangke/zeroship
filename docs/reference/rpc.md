# `@zeroship/rpc`

`@zeroship/rpc` is the public RPC package for server procedures and typed
clients. It has three stable subpaths:

- `@zeroship/rpc/server` — server-side procedure wrappers.
- `@zeroship/rpc/client` — manual clients and the runtime used by generated stubs.
- `@zeroship/rpc/types` — type-only contract helpers.

Two companion packages appear in the examples below:

- `@zeroship/vite-plugin` — the build plugin (`zeroship()` in `vite.config.ts`)
  that discovers `"use server"` modules and turns client imports of wrapped
  exports into RPC calls. Generated stubs and hand-written clients then share
  the same transport, transformer, retry and error behavior.
- `@zeroship/server` — `defineApp` for the app's resource policy, plus `z` and
  the runtime helpers `runQuery`, `runMutation` and `currentSignal`.

## Server authoring

Server modules opt into discovery with a file-level `"use server"` directive.
The plugin scans every `.ts`, `.tsx`, `.js` and `.jsx` module in your project
(everything under `node_modules` is skipped), and a file becomes a server module
only when that directive is its first statement. There is no required folder,
and a path such as `src/server/` does not opt a file in on its own. Only exports
wrapped with a procedure helper are published as RPC endpoints; plain helper
exports stay private. Every published procedure needs an explicit `id`; a
production build refuses one whose id was defaulted from its export name.

Handlers reach the runtime environment through `env`, imported from `zeroship`.
`env` is the per-request environment: the collections you declared in
`migrations/` appear on `env.db`, app secrets appear as string keys, and the
storage and key-value namespaces appear under their own keys.

- `env.db.<collection>.find(filter?, options?)` returns a chainable query.
  Awaiting it resolves to `{ data, error }`: `data` is the matching rows (or
  `null`) and `error` is non-null when the read failed. Chain `.sort(...)`,
  `.limit(...)`, `.skip(...)`, `.select(...)` or `.with(...)` before awaiting.
- `env.db.<collection>.insert(row)` resolves to `{ data, error }`: `data` is
  the inserted row, with generated fields such as `id`, `created_at` and
  `version` filled in.
- `env.db.live(queryFn, options?)` returns an async iterable that yields a
  fresh result array: the initial result, then a new array on every change to
  a table the query reads. Call `.close()` (or `break` out of the loop) to tear
  the subscriptions down. When `queryFn` does not read through `env.db`, pass
  `{ tables: [...] }` so the watched tables are known.

`@zeroship/db` is the full database SDK; `env.db` is its runtime surface. The
same operations are available inside `env.db.transaction(async (tx) => ...)`,
where `tx.<collection>` methods return values directly and throw on failure.

```ts
"use server";

import { query, mutation, stream } from "@zeroship/rpc/server";
import { env } from "zeroship";

export const listTodos = query(
  async (input: { userId: string }) => {
    const { data, error } = await env.db.todos.find({ userId: input.userId });
    if (error) throw error;
    return data ?? [];
  },
  { id: "todos.list" },
);

export const createTodo = mutation(
  async (input: { userId: string; title: string }) => {
    const { data, error } = await env.db.todos.insert({
      userId: input.userId,
      title: input.title,
      done: false,
    });
    if (error) throw error;
    return data;
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

Because the wire ID and the export name are independent, calling one by the
other's name fails, and the error does not tell you that. `listTodos` above is
served at `todos.list`; a request to `/__zeroship/v1/listTodos` returns

```json
{ "message": "Method not found: listTodos", "name": "Error", "code": "NOT_FOUND" }
```

which is correct — that ID genuinely does not exist — but reads like the
procedure is missing rather than renamed. If you get `NOT_FOUND` for a procedure
you are sure you exported, check the ID before anything else.

The active wrapper helpers are:

- `procedure(handler, { kind, id, ... })`
- `query(handler, { id, ... })`
- `mutation(handler, { id, idempotent, ... })`
- `action(handler, { id, ... })`
- `stream(handler, { id, ... })`
- `subscription(handler, { id, ... })`

Give a handler a required input parameter when the procedure takes an input
object: an optional parameter (`input?: ...`) makes the wrapper infer a
no-input procedure, and callers then cannot pass arguments. A `query` is
read-only and cannot call `fetch()`; reach for `action` when a handler needs an
outbound HTTP call. `subscription` metadata is recognized by discovery, but the
public generated/manual client shape does not expose subscriptions yet — use
`stream(...)` for shipped live feeds.

## Procedure auth (authenticated by default)

Once deployed, **every RPC procedure requires an authenticated end-user by
default** — a procedure whose policy chain declares no `auth` resolves to
`auth: "user"`, not public. This is fail-closed by design: forgetting
to set auth yields a loud `401`, never a silent public endpoint.

Read the caller inside a handler with `auth` from `@zeroship/auth`:

```ts
import { auth } from "@zeroship/auth";

export const me = query(
  async () => {
    const user = auth.getUser(); // User | null
    if (!user) return null;
    return { id: user.id, name: user.name, avatar: user.avatar };
  },
  { id: "account.me", auth: "user" },
);
```

`getUser()` returns the authenticated user or `null`. `requireUser()` returns
the same user or throws a `401` error carrying `code: "UNAUTHENTICATED"`. A user
has `id`, `email`, `emailVerified`, `name`, `avatar` and `scopes`.
`isLoggedIn()` is a convenience boolean.

`auth` is a field on the procedure's config object — the second argument every
wrapper helper takes; the `{ id, ... }` shorthand in the helper list stands for
the full config. Declaring `auth: "user"` is what protects the route, and it is
also the fail-closed default, so naming it is optional but makes the intent
local. The app's resource policy declares the same field for a family or a
single procedure, and for the same id it is merged over the procedure's own
config. `requireUser()` reads the identity; it is not the gate — you do not have
to call it to be protected.

To make a procedure **publicly reachable**, opt in explicitly in the app's
resource policy. A fresh scaffold ships no `src/server/config.ts` — create it:

```ts
import { defineApp } from "@zeroship/server";

export default defineApp({
  resources: {
    // A family default: every procedure whose id is `wizard` or starts
    // with `wizard.` (such as `wizard.list`) resolves to anonymous.
    "rpc:wizard": { auth: "anonymous", publiclyAccessible: true },
  },
});
```

A resource key is `rpc:` followed by a procedure id. `rpc:wizard` is a family:
it covers the procedure whose id is exactly `wizard` and every procedure whose
id continues with a `.`-separated segment, such as `wizard.list` or
`wizard.suggest`. It does not match `wizardlist`, nor an id in another
namespace. Target one procedure by naming its full id, `"rpc:wizard.list"`. The
build already derives an `rpc:<id>` entry for every published procedure and
merges your tree over those entries, so a config entry wins for the keys it
names.

`publiclyAccessible: true` is the deliberate confirmation the build requires
alongside `auth: "anonymous"` — it makes "this endpoint is intentionally public"
explicit and reviewable. It is a resource-tree field only: the per-procedure
config accepts `auth` but not `publiclyAccessible`, so a public endpoint is
declared here in the app config, not on the wrapper.

> **`"anonymous"` and `"user"` are the only two values `auth` accepts.** The
> build refuses anything else, in dev as well as production. Inheritance is a
> boolean OR: if any resource in a family's chain requires a user, the whole
> family requires one. A child weakens that only by naming the inherited field
> in its own `override` array, placed on the same resource node:
>
> ```ts
> "rpc:wizard.list": {
>   auth: "anonymous",
>   publiclyAccessible: true,
>   override: ["auth"],
> },
> ```
>
> `override` lists the inherited field names this entry deliberately
> re-declares. Weakening `auth` from a parent's `"user"` to `"anonymous"`
> without it is a build error; strengthening `auth` to `"user"` needs no
> marker.

`pnpm dev` does not enforce this policy, so a procedure that is gated in
production works locally for every caller. Test an auth decision against a
deploy, not against dev.

## Vite-generated calls

With `@zeroship/vite-plugin`, client code imports the server export directly:

```ts
import { createTodo, listTodos } from "./index";

const todos = await listTodos({ userId });
const todo = await createTodo({ userId, title: "Ship docs" });
```

The transform rewrites those imports to callable procedure references created
by `@zeroship/rpc/client`. Generated stubs and manual clients therefore share
the same behavior: same-origin `/__zeroship/v1/<id>` by default, configurable
`baseUrl`, auth, headers, timeout, retry policy, idempotency, transformer, and
`RpcError` handling.

Idempotency is declared where the procedure is — `mutation(handler, { id,
idempotent: true })`. When a procedure is idempotent, the client mints an
`Idempotency-Key` automatically and reuses it across retries; you do not pass
one. Pass `{ idempotencyKey: "..." }` as a per-call option only when you need a
specific key, for example to make two separate calls dedupe against each other.
It overrides the auto-generated key.

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

const todos = await listTodos({ userId: "usr_..." });
await createTodo({ userId: "usr_...", title: "Write docs" });
```

`InferRpcContract<typeof server>` only works when TypeScript can see the server
procedure declarations. It is a type-only source of truth; it does not import
server code into the browser at runtime.

When the server declarations are not available, declare a contract by hand. The
helpers are `Query<Input, Output, Meta>`, `Mutation<Input, Output, Meta>` and
`Stream<Input, Output, Meta>`; `Input` defaults to `void`, `Output` to `unknown`,
and `Meta` is `{ idempotent: true }` or omitted. `Action` and `Subscription`
follow the same shape.

```ts
import type { Query, Mutation, Stream } from "@zeroship/rpc/types";

type Todo = { id: string; userId: string; title: string; done: boolean };
type TodoSnapshot = { kind: "snapshot"; rows: Todo[] };

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
  "todos.subscribe": { kind: "stream" },
} as const;

const rpc = createRpcClient<AppRpc>({ procedures, baseUrl: "https://..." });
await rpc.call("todos.list", { userId });
```

## Transport

The shipped wire path is `/__zeroship/v1/<wireId>`.

| Kind | Request shape |
| --- | --- |
| `query` | `GET /__zeroship/v1/<id>?input=<base64url-json>`. When that URL would exceed about 6 KB, the client sends `POST` with the same JSON body and an `X-Method: GET` header instead. The platform still routes it as a query, so a handler never reads the header. |
| `mutation` | `POST /__zeroship/v1/<id>` with a JSON body. |
| `action` | Same wire shape as a mutation; the handler may also call `fetch()`. |
| `stream` | `POST /__zeroship/v1/<id>` with `Accept: text/event-stream`; the client reads the response as a stream of the values the handler yields. |

The client sets `X-Request-Id` on every request, and sets `Idempotency-Key` on
idempotent writes. You do not set either header yourself. An auth resolver
supplies `Authorization: Bearer <token>` when it returns a token.

## Transformers

The server always uses the `"json"` transformer; leave the client's at its
default. A response carries the procedure's return value under `json`, and
values plain JSON cannot represent exactly — a `Date`, for example — are
annotated in a sibling `meta` block:

> `{"json": { ... }, "meta": { "values": { "entries.0.modifiedAt": ["Date"] }, "v": 1 }}`

The RPC client decodes this for you; a caller parsing raw responses must read
`json` and apply `meta`.

`"superjson"` exists as a client option for rich values such as `Date`, `BigInt`,
`Map`, `Set`, `URL` and typed arrays, but it only round-trips when the server is
configured to speak the same transformer. The platform does not expose that
configuration to creators today, so a manual client that selects `"superjson"`
will not decode against a deployed app.

## Errors and retries

All client failure paths throw `RpcError` from `@zeroship/rpc/client`:

```ts
class RpcError extends Error {
  name: "RpcError";
  code: ErrorCode;
  message: string;
  details?: unknown;
  retryable: boolean;
  status?: number;
  trace_id?: string;
}
```

`isRpcError(err)` is the type guard, and `ErrorCode` is importable as a frozen
enum-like object. Branch on `error.code`, not on the message.

`code` is a closed set:

| `error.code` | Retried by default |
| --- | --- |
| `UNAUTHENTICATED` | no |
| `PERMISSION_DENIED` | no |
| `NOT_FOUND` | no |
| `INVALID_ARGUMENT` | no |
| `FAILED_PRECONDITION` | no |
| `ALREADY_EXISTS` | no |
| `RESOURCE_EXHAUSTED` | yes |
| `ABORTED` | no |
| `INTERNAL` | no |
| `UNAVAILABLE` | yes |
| `TIMEOUT` | yes |
| `CANCELLED` | no |
| `OUT_OF_RANGE` | no |
| `UNIMPLEMENTED` | no |

The client takes `retryable` from the server's envelope when present and falls
back to those defaults. A `RpcError` also carries `details` when the server
supplied them, `status` when the failure came over HTTP, and `trace_id` when the
server stamped one.

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

Retry policy applies to unary calls. Reads retry when the error is retryable.
Writes retry only when the procedure is `idempotent: true` or `retryWrites: true`
is set; an idempotent write carries a stable `Idempotency-Key`, so a retry
dedupes instead of running the call twice. The default is no retry
(`attempts: 1`); `retry: true` means 3 attempts, and `retry: { attempts,
baseDelayMs, maxDelayMs, jitter, retryWrites }` customizes it.

### Idempotency

A mutation opts in with `idempotent: true`; it may also set
`idempotencyTtl: { hours }` (default 24 hours, minimum 1, maximum 168).

**The first response your handler produces is stored under the key and replayed
verbatim** to every later request carrying the same key and the same input, for
the procedure's idempotency TTL. The same key with different input is refused
with `409 ALREADY_EXISTS` and a `Retry-After` header.

The stored response belongs to the caller who produced it. Keys are scoped per
signed-in user, so two people can use the same key on the same procedure and
each gets their own result. On procedures declared `auth: "anonymous"` there is
no signed-in user to scope by, so every anonymous caller shares one keyspace and
the key must be unguessable: pass a UUIDv4 or UUIDv7 in the canonical hyphenated
form (`crypto.randomUUID()` produces one). Anything else is rejected with `400
INVALID_ARGUMENT` and `details.reason:
"anonymous_idempotency_key_must_be_uuid_v4_or_v7"` before your handler runs.
Authenticated procedures accept any non-empty key, so a natural key such as an
order id is fine there.

**Only a response your handler produced is stored.** When the platform answers
on its own behalf instead of running your call — it could not reach your app, or
the request needs a sign-in round trip first — the key stays open and the retry
runs the call for real. That is why a redirect into the sign-in flow is never
remembered: it says nothing about your mutation, and its one-time sign-in state
would be stale by the time anyone replayed it. A redirect *your handler* returns
is a genuine outcome and is stored and replayed like any other.

**A `5xx` is never stored**, even one your handler returned. A `5xx` means no
outcome was reported, so the key stays open and a retry re-runs the call.
Rejections you want remembered must be `4xx`: return `400`/`409`/`422` and the
retry gets that same answer back without re-executing.

## Streams

For stream procedures, call the generated or manual function and iterate:

```ts
for await (const snapshot of subscribeTodos({ userId })) {
  render(snapshot);
}
```

For integrations that want a URL instead of an iterator, stream procedures expose
`streamUrl(input)`. It returns the URL as a string when the procedure takes no
input, or a `Promise<string>` when the input is encoded into the URL.

`streamUrl` returns only the URL: it attaches no headers, and the client's
`auth` resolver is not consulted for it. Consume an authenticated stream through
the iterator call shown above, which sends `Authorization: Bearer`. A consumer
that fetches the URL itself must supply that header from your sign-in flow, so
use `streamUrl` for streams that need no header auth or for a consumer you
configure with the header.

The handler's iterator is advanced only as the caller consumes values. When the
caller disconnects, cancels, or the request times out, `currentSignal()` aborts
and the iterator's `return()` is called, so `finally` blocks run. A timeout
arrives on the client as the `TIMEOUT` error code.

Get the signal with `currentSignal()` from `zeroship` (also re-exported by
`@zeroship/server`); it returns the `AbortSignal` for the in-flight request.

```ts
import { currentSignal } from "zeroship";

export const watch = stream(
  async function* () {
    const signal = currentSignal();
    // Use `signal` to stop external work when the caller disconnects.
  },
  { id: "todos.watch" },
);
```

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