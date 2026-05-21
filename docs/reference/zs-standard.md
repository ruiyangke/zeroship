# The zeroship deploy contract

**Status:** Reference

## TL;DR

A deployable zeroship app is a JS module exporting

```js
export default { schema?, fetch?, rpc? }
```

All three keys are optional. Tooling (Vite, esbuild, swc) is optional.
The runtime owns dispatch.

```js
// minimal HTTP-only — runnable today via `zeroship serve myapp.js`
export default {
  fetch(request) { return new Response("hi"); }
};
```

## The export shape

```ts
interface ZeroshipApp {
  /** Database schema. Runtime installs at boot via `installSchema`. */
  schema?: Record<string, SchemaShape>;

  /** WinterCG HTTP handler. Called for any URL that isn't /_zs/v1/<id>. */
  fetch?: (request: Request, env: Env, ctx: Ctx) => Response | Promise<Response>;

  /** RPC dispatch table. URL `/_zs/v1/<wireId>` invokes `rpc[wireId]`. */
  rpc?: Record<string, Procedure> | ((name, input, ctx) => unknown);

  /** Optional non-WinterCG fast HTTP path. See "fetchFast" below. */
  fetchFast?: (method, url, body, env) => unknown;
}

type Procedure =
  & ((input: unknown, ctx: Ctx) => unknown | Promise<unknown> | AsyncIterable<unknown>)
  & { config?: ProcedureConfig };

interface ProcedureConfig {
  kind?: "query" | "mutation" | "action" | "stream" | "subscription";
  input?: { parse: (v: unknown) => unknown };   // Zod-compatible
  output?: { parse: (v: unknown) => unknown };  // dev-only validation
  id?: string;                                  // wireId override
  isolation?: "READ COMMITTED" | "REPEATABLE READ" | "SERIALIZABLE";
}
```

Dispatch precedence: a request to `/_zs/v1/<id>` routes through
`default.rpc[id]`; any other URL routes through `default.fetch`.

## Examples

All examples below are runnable. The four `examples/*.js` files are
checked into the repository and exercised by smoke scripts.

### Minimal HTTP-only

`examples/http-handler.js` — only `default.fetch`, no schema, no RPC:

```js
export default {
  async fetch(request) {
    const url = new URL(request.url);
    if (url.pathname === "/health") return new Response("OK");
    return Response.json({ error: "Not Found" }, { status: 404 });
  },
};
```

Run: `zeroship serve examples/http-handler.js --port 3000`.

### Pure RPC dict

`examples/raw-rpc.js` — dict-shape `default.rpc`, no Vite, no SDK:

```js
const ping = () => "pong";
const echo = (input) => ({ echo: input });

const status = () => ({ ok: true, ts: Date.now() });
status.config = { kind: "query" };

export default {
  rpc: { ping, echo, status },
};
```

Call: `curl -X POST http://localhost:3000/_zs/v1/ping -d '{"json":null}'`.

### Streaming via subscription

`examples/raw-streaming.js` — async generator returned from a procedure
with `config.kind = "subscription"`:

```js
const tick = async function* (input) {
  const count = input?.count ?? 5;
  for (let i = 0; i < count; i++) {
    yield { i, ts: Date.now() };
    await new Promise((r) => setTimeout(r, 10));
  }
};
tick.config = { kind: "subscription" };

export default { rpc: { tick } };
```

The kernel's WS-upgrade path on `/_zs/v1/tick` calls the dispatcher and
pumps each yield as a `{"t":"data",value}` frame. See
`examples/raw-streaming.smoke.sh` for the unary surfaces it covers and
`crates/runtime/src/core/init.rs::dispatchSubscription` for the wire.

### DB + RPC + SSR

`examples/db-todos/src/index.ts` — the canonical Vite-built demo using
`@zeroship/db`, `@zeroship/server` wrappers, and SSR:

```ts
"use server";
import { t, schema } from "@zeroship/db";
import { env } from "zeroship";
import { query, mutation } from "@zeroship/server";

const dbSchema = {
  users: { email: t.string().required().unique(), name: t.string().required() },
  todos: schema({
    userId: t.ref("users").required(),
    title:  t.string().required().min(1).max(200),
    done:   t.boolean().default(false),
  }),
};

export default { schema: dbSchema };

export const listTodos = query(async ({ userId }) => {
  const { data } = await env.db.todos.find({ userId });
  return data ?? [];
});

export const addTodo = mutation(async (input) => {
  const { data } = await env.db.todos.insert(input);
  return data;
});
```

The Vite plugin (`@zeroship/vite-plugin`) walks the `"use server"`
named exports and emits a synthetic entry that re-exports
`default = { schema, fetch, rpc }`. The runtime sees the same standard
shape as raw deploys do.

## Field-by-field reference

### `default.schema`

A record of `{ collectionName: shape }`. At boot the runtime imports the
user entry, reads `default.schema` synchronously, and calls
`installSchema(schema, env.db)` from `@zeroship/db`. This:

1. Issues idempotent DDL (CREATE TABLE / ALTER TABLE / CREATE INDEX
   CONCURRENTLY) against the app's Postgres schema.
2. Installs typed `Collection` wrappers (and the `transaction` / `live`
   extension methods) as own properties on `env.db.<name>`.
3. Returns `{ collections, ready }` — the bootstrap awaits the `ready`
   promise so module evaluation gates on DDL settling.

User code never calls `installSchema` directly — declare schema on the
entry and the platform handles registration.

See `docs/reference/db.md` for the schema builder surface (`t.string()`,
`t.ref(...)`, `schema().withVersioning()`, named indexes, etc.).

### `default.fetch`

WinterCG handler. Signature: `(request, env, ctx) => Response | Promise<Response>`.

Dispatch precedence:

1. `/_zs/v1/<wireId>` requests route through `default.rpc` (not
   `default.fetch`). The kernel never builds a `Request` for the
   RPC fast path.
2. Everything else hits `default.fetch`.

When the module exports neither `default.fetch` nor `default.rpc`,
the runtime's `fallbackFetch` answers RPC URLs via the subscription
WS-upgrade path (if applicable) and returns 404 for the rest.

### `default.rpc`

Dispatch table for `/_zs/v1/<wireId>` traffic. Two shapes are accepted:

| Shape | When to use | Wire |
|---|---|---|
| **Dict** `{ [wireId]: handler }` | Canonical for production bundles and raw deploys. | Runtime wraps in `__zsDispatch`. |
| **Function** `(name, input, ctx) => unknown` | Advanced — dynamic routing, dev-bootstrap (HMR), multi-tenant prefix dispatch. | Used directly. Dispatcher logic is the caller's responsibility. |

For dict-shape, the runtime owns:

- input validation via `fn.config.input.parse()`
- capability frame via `__zsEnterKind` / `__zsExitKind`
- auto-tx for `kind: "query" | "mutation"`
- AsyncIterator stream-framing tag (`__zsOutputIsString`)
- 404 NOT_FOUND envelope on unknown wireId
- dev-only output validation via `__zsValidateOutput`

See `crates/runtime/src/bootstrap/rpc_dispatch.js` for the canonical
dispatcher source.

### `default.fetchFast` (opt-in fast path)

A non-WinterCG fast HTTP entry. Signature:

```ts
fetchFast(method: string, url: string, body: unknown, env: Env):
  | { status: number, headers: Record<string,string>, body: unknown }
  | string
  | Uint8Array
  | null  // null → kernel falls through to default.fetch
```

The kernel calls `fetchFast` BEFORE constructing a `Request` /
`Response`, skipping the WinterCG object wrappers for hot paths. Use
when microseconds matter; return `null` to defer back to `default.fetch`.

`fetchFast` is independent of `default.fetch` and `default.rpc` —
sibling, not a wrapper. `/_zs/v1/<id>` traffic always uses
`default.rpc`; `fetchFast` only sees non-RPC URLs.

## Procedure metadata

`fn.config = { kind, input, output, id, isolation }` is the contract for
per-procedure metadata. The dispatcher reads it; you attach it any way
you like.

### Canonical attachment: `@zeroship/server` wrappers

```ts
import { query, mutation, action, stream, subscription } from "@zeroship/server";

export const listTodos = query(async ({ userId }) => { /* ... */ });
export const addTodo   = mutation(async (input) => { /* ... */ });
export const callAi    = action(async (prompt) => { /* fetch */ });
export const watch     = subscription(async function* () { /* yield */ });
```

Each wrapper is `Object.assign(fn, { config: { kind, ...opts } })`.
Aliasing through these is the idiomatic surface and is recognised by
the Vite plugin's transform.

### Direct attachment (no SDK import)

```js
const addUser = (input) => env.db.users.insert(input);
addUser.config = { kind: "mutation" };
```

Equivalent at the dispatch layer. Use when avoiding the SDK import
(raw JS deploys; bench surfaces; runtime fixtures).

### Fields

| Field | Effect |
|---|---|
| `kind` | Capability bucket. `query`/`mutation` open an auto-tx; `query` is READ ONLY; `mutation` is SERIALIZABLE by default. `action` runs with no tx (use `runQuery`/`runMutation` to scope). `stream`/`subscription` carry no auto-tx. |
| `input` | Object with a `.parse(v)` method (Zod-compatible). Pre-validated by the dispatcher; throw → 400 INVALID_ARGUMENT with `details.issues`. |
| `output` | Same shape. Validated post-handler ONLY when `globalThis.__zsValidateOutput` is set (dev-only flag). |
| `id` | Override wireId. Default = property key under `default.rpc`. |
| `isolation` | Postgres isolation override for query/mutation. `"READ COMMITTED" \| "REPEATABLE READ" \| "SERIALIZABLE"`. |

## Wire format

| URL | Method | Dispatch |
|---|---|---|
| `POST /_zs/v1/<wireId>` | POST | `default.rpc[wireId](input, ctx)`; body is `{"json": <input>}` |
| `GET /_zs/v1/<wireId>?input=<base64url-json>` | GET | Same; input from query param |
| `/_zs/v1/<wireId>` WebSocket upgrade | GET | Subscription path — `dispatchSubscription` invokes `default.rpc[wireId](input, ctx)` and pumps each yielded value as a `{"t":"data",value}` frame |
| anything else | any | `default.fetch(request, env, ctx)` |

Response envelope:

- Success: `200 { "json": <return value> }`. `undefined` becomes `null`.
- Handler throws: `<status> { message, name, code?, details?, retryable? }`.
  Status defaults to 500; override by setting `err.status`.
- Streaming (AsyncIterator return over POST/SSE): AI-SDK Data Stream
  framing — `0:`/`2:`/`e:`/`d:` line-prefixed.

## Without Vite — raw JS deploy

Two shipped examples demonstrate the contract end-to-end with no
tooling at all:

- `examples/raw-rpc.js` — three trivial procedures + `config.kind`.
  Smoke is inline in the file header.
- `examples/raw-streaming.js` — query with `input.parse`, action with
  outbound fetch, mutation, async-generator subscription.
  `examples/raw-streaming.smoke.sh` exercises four surfaces.

Run:

```bash
zeroship serve examples/raw-rpc.js --port 3000
curl -X POST http://localhost:3000/_zs/v1/ping -d '{"json":null}'
# → {"json":"pong"}
```

Other raw-JS deploys in `examples/`:

- `examples/http-handler.js` — `default.fetch` only
- `examples/url-shortener.js` — `default.fetch` + `env.KV` bindings
- `examples/jwt-validator.js`, `examples/weather-proxy.js`,
  `examples/ai-streaming.js` — `default.fetch`-only handlers

## With Vite — the plugin's contribution

`@zeroship/vite-plugin` is convenience, not requirement. It contributes:

- **Source-style ergonomics.** `export const list = query(async () => ...)`
  named exports on `"use server"` files are walked into a synthetic
  `default.rpc` dict at build time. See `docs/reference/vite-plugin.md`.
- **HMR.** Dev-bootstrap re-imports through Vite's `ModuleRunner` per
  request; schema/rpc edits land without restart.
- **Asset pipeline.** Client bundles, CSS, SSR, the `zeroship` virtual
  module.

The synthetic SSR entry (Stage 5b) is a thin normaliser:

```js
// virtual:zeroship/_server-entry — generated by the plugin
import * as _zsUser from "<user entry>";
const _zsUserDefault = (_zsUser?.default && typeof _zsUser.default === "object")
  ? _zsUser.default : {};

const _zsRpc = (typeof _zsUserDefault.rpc === "object" && _zsUserDefault.rpc)
  ? { ..._zsUserDefault.rpc } : {};
for (const name of Object.keys(_zsUser)) {
  if (name === "default" || name === "fetch") continue;
  const fn = _zsUser[name];
  if (typeof fn !== "function") continue;
  const id = (fn.config?.id && typeof fn.config.id === "string") ? fn.config.id : name;
  _zsRpc[id] = fn;
}

export default {
  schema: _zsUserDefault.schema,
  fetch:  _zsUserDefault.fetch ?? _zsUser.fetch,
  rpc:    _zsRpc,
};
```

~30 lines, no dispatcher source. Production bundles consume the runtime
dispatcher just like raw deploys do.

## Behavior the runtime owns

For dict-shape `default.rpc`, the embedded dispatcher
(`crates/runtime/src/bootstrap/rpc_dispatch.js`) handles:

1. **Input validation** — `fn.config.input.parse(input)`. Throws are
   coerced to `400 INVALID_ARGUMENT` with `details.issues = err.issues ?? err.errors ?? []`.
2. **Capability frame** — `__zsEnterKind(kind)` / `__zsExitKind(token)`.
   Refuses cross-kind violations natively (e.g. `fetch` inside a
   `mutation`).
3. **Auto-tx** — for `kind: "query" | "mutation"` with plugin-db loaded,
   wraps the handler in `__zsBeginAutoTx` / `__zsEndAutoTx`. Commits on
   success, rolls back on throw. Commit failure becomes the
   caller-visible error.
4. **AsyncIterator framing tag** — sets `result.__zsOutputIsString` when
   `cfg.output` is a Zod string schema, so the SSE encoder picks the
   AI-SDK `0:` (text) lane over the `2:` (object) lane.
5. **404 NOT_FOUND** — unknown wireId returns
   `404 { message: "Method not found: <name>", code: "NOT_FOUND" }`.
6. **Output validation (dev only)** — `fn.config.output.parse(result)`
   runs when `globalThis.__zsValidateOutput` is set. Throws become
   `500 INTERNAL` with `details.issues`.

## Advanced: function-shape `default.rpc`

For users who need custom dispatch — dynamic routing, multi-tenant
prefix matching, audit wrappers, dev HMR — `default.rpc` can be a
function:

```js
const namespaces = {
  todos: { list: () => /* ... */, add: () => /* ... */ },
  users: { list: () => /* ... */ },
};

function customDispatch(name, input, ctx) {
  const [ns, method] = name.split(".");
  const fn = namespaces[ns]?.[method];
  if (typeof fn !== "function") {
    const err = new Error("Method not found: " + name);
    err.status = 404;
    err.code = "NOT_FOUND";
    throw err;
  }
  // The caller owns input validation, capability frames, auto-tx, etc.
  // for this branch — the runtime dispatcher does not run.
  return fn(input, ctx);
}

export default { rpc: customDispatch };
```

The runtime checks `typeof rpc === "function"` first; otherwise treats
it as a dict. The dev-bootstrap exports function-shape because the user
module's namespace re-resolves per request under HMR; production
bundles emit dict-shape because the bundle is frozen at build time.

Default to dict-shape. Use function-shape only when you need the
flexibility — you give up the runtime's built-in validation, auto-tx,
and capability machinery, and must re-implement what you need.

## See also

- `docs/reference/db.md` — `@zeroship/db` SDK, schema builders, CRUD
- `docs/reference/vite-plugin.md` — `"use server"` discovery, transform rules
- `docs/reference/zship.md` — `.zship` deploy artifact format
- `docs/reference/websocket-design.md` — WS subscription wire
- `docs/proposals/zs-standard-and-vite-v2.md` — design history
- `crates/runtime/src/bootstrap/rpc_dispatch.js` — canonical dispatcher
- `crates/runtime/src/core/init.rs` — `default.rpc` / `default.fetch` /
  `default.fetchFast` wiring
- `crates/runtime/tests/rpc_dispatch.rs` — dict + function-shape tests
