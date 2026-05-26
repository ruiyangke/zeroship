# RPC client inferred contracts

**Status:** Shipped subset; live reference is [`docs/reference/rpc.md`](../reference/rpc.md)
**Scope:** `@zeroship/rpc`, `@zeroship/vite-plugin`

This proposal is retained as design history. The shipped package path is
`@zeroship/rpc/types`, not `@zeroship/types/rpc`; manual and generated clients
both use `@zeroship/rpc/client`.

## Motivation

The current strongly typed client shape can be made precise, but a hand-written contract is noisy:

```ts
export type AppRpc = {
  "todos.list": Query<{ limit?: number }, Todo[]>;
  "todos.create": Mutation<{ text: string }, Todo, { idempotent: true }>;
  "chat.ask": Stream<{ prompt: string }, ChatChunk>;
};
```

This duplicates information already present in the backend procedure declarations: the wire ID, procedure kind, input type, output type, and metadata such as idempotency. Zeroship should let users infer the client contract from the backend RPC module when those functions are available to TypeScript.

The goal is not to make the browser import server code at runtime. The goal is to use server procedure types as the source of truth and keep runtime metadata generated or passed as data.

## Goals

- Infer client input, output, kind, ID, and metadata from backend procedure exports.
- Put shared procedure brands in a type-only RPC module so the browser client never imports server runtime code.
- Preserve explicit production wire IDs from wrapper config, for example `{ id: "todos.list" }`.
- Support non-Vite users through the public `@zeroship/rpc/client` subpath.
- Keep one client surface for generated and manual usage: `createRpcClient`.
- Narrow client methods by procedure kind: `rpc.query("todos.create")` must be a type error.
- Avoid exposing framework internals such as `__callProcedure` as public root exports.
- Keep runtime policy honest: retries, idempotency, stream transport, headers, auth, and timeout must be configurable without pretending erased types exist at runtime.

## Non-goals

- No implicit production IDs from export names.
- No runtime import of server modules into browser bundles.
- No implicit runtime metadata from a generic parameter. Type inference gives types; generated or explicit data configures runtime behavior.
- No cross-language schema generation in this proposal.
- No compatibility aliases for old client shapes.
- No attempt to infer arbitrary `Response` bodies as unary JSON RPC results.

## Proposed authoring model

Server RPC exports remain the source of truth:

```ts
"use server";

import { env } from "zeroship";
import { mutation, query, stream } from "@zeroship/rpc/server";

type Todo = {
  id: string;
  text: string;
};

export const listTodos = query(
  async (input: { limit?: number }): Promise<Todo[]> => {
    return env.db.todos.find({ limit: input.limit ?? 20 });
  },
  { id: "todos.list" },
);

export const countTodos = query(
  async (): Promise<number> => {
    return env.db.todos.count();
  },
  { id: "todos.count" },
);

export const createTodo = mutation(
  async (input: { text: string }): Promise<Todo> => {
    return env.db.todos.insert({ text: input.text });
  },
  { id: "todos.create", idempotent: true },
);

export const askChat = stream(
  async function* (input: { prompt: string }): AsyncIterable<{ token: string }> {
    yield* runModel(input.prompt);
  },
  { id: "chat.ask" },
);
```

Generated contract modules can infer the contract without repeating the shape. The generated file imports the server module as a type only and emits a small runtime metadata object:

```ts
// rpc.gen.ts
import { defineRpcProcedures, type InferRpcContract } from "@zeroship/rpc/client";
import type * as serverRpc from "../server/rpc";

export type AppRpc = InferRpcContract<typeof serverRpc>;

export const procedures = defineRpcProcedures<AppRpc>()({
  "todos.list": { kind: "query" },
  "todos.count": { kind: "query" },
  "todos.create": { kind: "mutation", idempotent: true },
  "chat.ask": { kind: "stream" },
});
```

Manual client code then gets both strong types and runtime behavior:

```ts
import { createRpcClient } from "@zeroship/rpc/client";
import { procedures, type AppRpc } from "./rpc.gen";

const rpc = createRpcClient<AppRpc>({
  baseUrl: "https://example.zeroship.ai",
  auth: () => localStorage.getItem("token"),
  procedures,
});

const listTodos = rpc.query("todos.list");
const countTodos = rpc.query("todos.count");
const createTodo = rpc.mutation("todos.create");
const askChat = rpc.stream("chat.ask");

const todos = await listTodos({ limit: 10 });
const total = await countTodos();
const todo = await createTodo({ text: "Ship inferred RPC types" });

for await (const chunk of askChat({ prompt: "Summarize this task" })) {
  console.log(chunk.token);
}
```

In a Vite app, users should not need to create this module by hand. The Vite plugin already has the server procedure registry and can emit equivalent stubs for client imports.

These should be compile-time errors:

```ts
rpc.query("todos.create");
rpc.mutation("chat.ask");
listTodos({ limit: "10" });
createTodo({});
```

Inferred contracts require explicit literal IDs in every mode. Dev-only implicit export-name IDs may still exist for local untyped dispatch, but they do not participate in `InferRpcContract` and production builds continue to reject them.

## Shared type owner

The procedure brand types should live with the RPC package, not in a separate global types package. Use a type-only subpath:

```ts
import type {
  InferRpcContract,
  RpcKind,
  ServerProcedure,
} from "@zeroship/rpc/types";
```

`@zeroship/rpc/server` imports those types and returns branded procedures from `query`, `mutation`, `action`, `stream`, and `subscription`. It should not export `env`, request state, storage, database, auth, or other platform substrate. Those stay in `zeroship` or broader server SDK surfaces. `@zeroship/rpc/client` imports the same types to implement and re-export `InferRpcContract`.

This keeps the ownership local to RPC while preserving dependency hygiene:

- `@zeroship/rpc/types` is type-only and browser-safe.
- `@zeroship/rpc/server` can depend on the RPC types without pulling in the client.
- `@zeroship/rpc/client` can depend on the RPC types without pulling in server runtime code.
- Users learn one product namespace: `@zeroship/rpc`.

The public import layout should be:

```ts
import { createRpcClient, type InferRpcContract } from "@zeroship/rpc/client";
import { query, mutation, stream } from "@zeroship/rpc/server";
import type { ServerProcedure } from "@zeroship/rpc/types";
```

The package should enforce this with subpath exports:

```json
{
  "name": "@zeroship/rpc",
  "exports": {
    "./client": {
      "types": "./dist/index.d.ts",
      "import": "./dist/index.js"
    },
    "./server": {
      "types": "./dist/server.d.ts",
      "import": "./dist/server.js"
    },
    "./types": {
      "types": "./dist/types.d.ts",
      "import": "./dist/types.js"
    }
  }
}
```

Subpath dependency rules:

- `@zeroship/rpc/client` must not import `@zeroship/rpc/server`.
- `@zeroship/rpc/server` must not import `@zeroship/rpc/client`.
- `@zeroship/rpc/types` is type-only. Its runtime JS should be an empty module.
- The root `@zeroship/rpc` import should stay empty or documentation-only until there is a strong reason for a root API.

Before launch, replace the split packages instead of aliasing them. The client and server wrapper surfaces live under `@zeroship/rpc/client` and `@zeroship/rpc/server`; `@zeroship/server` remains for app helpers such as `defineApp`, `z`, and runtime context utilities.

## Type model

Server wrappers should return callable values branded with erased type metadata. Use unique symbol phantom fields instead of public `__input` or `__output` fields.

```ts
declare const rpcKind: unique symbol;
declare const rpcId: unique symbol;
declare const rpcInput: unique symbol;
declare const rpcOutput: unique symbol;
declare const rpcMeta: unique symbol;

export type RpcKind = "query" | "mutation" | "action" | "stream" | "subscription";

type AsyncIterableItem<T> = T extends AsyncIterable<infer Item> ? Item : T;
type ProcedureOutput<Kind extends RpcKind, Output> = Kind extends "stream" | "subscription"
  ? AsyncIterableItem<Awaited<Output>>
  : Awaited<Output>;

type ProcedureHandler<Input, Output> = [Input] extends [void]
  ? () => Output | Promise<Output>
  : (input: Input) => Output | Promise<Output>;

export type NoClientRpcMeta = {
  readonly __clientRpcMeta?: never;
};

export type ClientRpcMeta = NoClientRpcMeta | {
  idempotent?: true;
};

type ClientRpcMetaFromConfig<Config> = Config extends { idempotent: true }
  ? { idempotent: true }
  : NoClientRpcMeta;

export type RpcProcedureRuntimeConfig<
  Kind extends RpcKind,
  Id extends string,
  Config extends object = {},
> = {
  id: Id;
  kind: Kind;
} & Config;

export type ServerProcedure<
  Kind extends RpcKind,
  Id extends string,
  Input,
  Output,
  Config extends object = {},
  ClientMeta extends ClientRpcMeta = ClientRpcMetaFromConfig<Config>,
> = ProcedureHandler<Input, Output> & {
  readonly config: RpcProcedureRuntimeConfig<Kind, Id, Config>;
  readonly [rpcKind]?: Kind;
  readonly [rpcId]?: Id;
  readonly [rpcInput]?: (input: Input) => void;
  readonly [rpcOutput]?: () => ProcedureOutput<Kind, Output>;
  readonly [rpcMeta]?: ClientMeta;
};
```

`config` may include server-only fields such as auth, rate limits, schemas, middleware, or timeouts. Only `ClientRpcMeta` is exposed through `InferRpcContract`; server-only config must not leak into client types.

Wrapper declarations preserve the literal ID, support no-input handlers, and extract only client-safe metadata:

```ts
export declare function query<
  const Id extends string,
  Output,
  const Config extends { id: Id },
>(
  handler: () => Output | Promise<Output>,
  config: Config,
): ServerProcedure<"query", Id, void, Awaited<Output>, Config>;

export declare function query<
  const Id extends string,
  Input,
  Output,
  const Config extends { id: Id },
>(
  handler: (input: Input) => Output | Promise<Output>,
  config: Config,
): ServerProcedure<"query", Id, Input, Awaited<Output>, Config>;

export declare function mutation<
  const Id extends string,
  Input,
  Output,
  const Config extends { id: Id; idempotent?: boolean },
>(
  handler: (input: Input) => Output | Promise<Output>,
  config: Config,
): ServerProcedure<"mutation", Id, Input, Awaited<Output>, Config>;

export declare function stream<const Id extends string, Item, const Config extends { id: Id }>(
  handler: () => AsyncIterable<Item>,
  config: Config,
): ServerProcedure<"stream", Id, void, AsyncIterable<Item>, Config>;

export declare function stream<
  const Id extends string,
  Input,
  Item,
  const Config extends { id: Id },
>(
  handler: (input: Input) => AsyncIterable<Item>,
  config: Config,
): ServerProcedure<"stream", Id, Input, AsyncIterable<Item>, Config>;
```

The shared type module exposes `InferRpcContract`:

```ts
export type InferRpcContract<TModule> = {
  [ExportName in keyof TModule as TModule[ExportName] extends ServerProcedure<
    any,
    infer Id extends string,
    any,
    any,
    any,
    any
  >
    ? Id
    : never]: TModule[ExportName] extends ServerProcedure<
    infer Kind extends RpcKind,
    any,
    infer Input,
    infer Output,
    any,
    infer ClientMeta
  >
    ? {
        kind: Kind;
        input: Input;
        output: ProcedureOutput<Kind, Output>;
        meta: ClientMeta;
      }
    : never;
};

export type ProcedureInput<Def> = Def extends { input: infer Input } ? Input : never;
export type ProcedureResult<Def> = Def extends { output: infer Output } ? Output : never;

type ProcedureInvokeArgs<Def> = [ProcedureInput<Def>] extends [void]
  ? [options?: ProcedureCallOptions<Def>]
  : [input: ProcedureInput<Def>, options?: ProcedureCallOptions<Def>];

type RpcCallArgs<Def> = [ProcedureInput<Def>] extends [void]
  ? [options?: ProcedureCallOptions<Def>]
  : [input: ProcedureInput<Def>, options?: ProcedureCallOptions<Def>];

export type UnaryProcedure<Def> = (
  ...args: ProcedureInvokeArgs<Def>
) => Promise<ProcedureResult<Def>>;

export type StreamProcedure<Def> = (
  ...args: ProcedureInvokeArgs<Def>
) => AsyncIterable<ProcedureResult<Def>>;
```

The client narrows IDs by kind:

```ts
type RpcIdsByKind<Contract, Kind extends RpcKind> = {
  [Id in keyof Contract]: Contract[Id] extends { kind: Kind } ? Id : never;
}[keyof Contract] &
  string;

export interface RpcClient<Contract> {
  query<Id extends RpcIdsByKind<Contract, "query">>(
    id: Id,
    options?: RpcProcedureOptions<Contract[Id]>,
  ): UnaryProcedure<Contract[Id]>;

  mutation<Id extends RpcIdsByKind<Contract, "mutation">>(
    id: Id,
    options?: RpcProcedureOptions<Contract[Id]>,
  ): UnaryProcedure<Contract[Id]>;

  action<Id extends RpcIdsByKind<Contract, "action">>(
    id: Id,
    options?: RpcProcedureOptions<Contract[Id]>,
  ): UnaryProcedure<Contract[Id]>;

  stream<Id extends RpcIdsByKind<Contract, "stream">>(
    id: Id,
    options?: RpcProcedureOptions<Contract[Id]>,
  ): StreamProcedure<Contract[Id]>;
}

type RpcCallableId<Contract> = {
  [Id in keyof Contract & string]: Contract[Id] extends { kind: "subscription" }
    ? never
    : Id;
}[keyof Contract & string];

export interface RegisteredRpcClient<Contract> extends RpcClient<Contract> {
  call<Id extends RpcCallableId<Contract>>(
    id: Id,
    ...args: RpcCallArgs<Contract[Id]>
  ): Contract[Id] extends { kind: "stream"; output: infer Output }
    ? AsyncIterableIterator<Output>
    : Promise<ProcedureResult<Contract[Id]>>;
}

export type RpcProcedureRegistry<Contract> = {
  [Id in keyof Contract & string]: Contract[Id] extends {
    kind: infer Kind extends RpcKind;
    meta: infer Meta extends object;
  }
    ? { kind: Kind } & Meta
    : never;
};

export type RpcProcedureOptions<Def> = Def extends {
  meta: infer Meta extends ClientRpcMeta;
}
  ? Meta
  : {};

type NoExtraProcedureKeys<Procedures, Contract> = Record<
  Exclude<keyof Procedures, keyof RpcProcedureRegistry<Contract>>,
  never
>;

export declare function defineRpcProcedures<Contract>(): <
  Procedures extends RpcProcedureRegistry<Contract>,
>(
  procedures: Procedures & NoExtraProcedureKeys<Procedures, Contract>,
) => Procedures;

export declare function createRpcClient<Contract>(
  options: RpcClientOptions<Contract> & {
    procedures: RpcProcedureRegistry<Contract>;
  },
): RegisteredRpcClient<Contract>;

export declare function createRpcClient<Contract>(
  options?: RpcClientOptions<Contract>,
): RpcClient<Contract>;
```

`defineRpcProcedures` is deliberately exact: generated or manual registries must include every inferred ID and must not contain extra typo keys. Per-procedure factory options use the same client-safe metadata, so `rpc.query("todos.list", { idempotent: true })` is a type error unless the procedure metadata allows it.

## Runtime metadata

Type inference alone cannot configure runtime behavior. TypeScript erases `idempotent`, kind metadata, and stream details. Zeroship needs one of these runtime metadata sources:

The `call(id, ...)` escape hatch is only present on `RegisteredRpcClient`, which means the client was created with a `procedures` registry. Without a registry, callers use kind-specific factories such as `rpc.query(id)` or `rpc.mutation(id, { idempotent: true })`, because those factories carry the kind at runtime. `call()` intentionally excludes `subscription` descriptors until the public subscription client API is designed; otherwise the type surface would promise behavior the generic client cannot deliver.

1. Vite generated stubs.
   The Vite plugin already sees server procedures. It should emit calls such as:

   ```ts
   import { createRpcClient } from "@zeroship/rpc/client";

   const rpc = createRpcClient();

   export const listTodos = rpc.query("todos.list");
   export const createTodo = rpc.mutation("todos.create", { idempotent: true });
   export const askChat = rpc.stream("chat.ask");
   ```

2. A generated non-Vite contract module.
   `zeroship rpc gen` should live in `@zeroship/cli` and reuse the same procedure extraction code as the Vite plugin. It emits a small runtime value plus inferred type:

   ```ts
   import type * as serverRpc from "../server/rpc";
   import { defineRpcProcedures, type InferRpcContract } from "@zeroship/rpc/client";

   export type AppRpc = InferRpcContract<typeof serverRpc>;

   export const procedures = defineRpcProcedures<AppRpc>()({
     "todos.list": { kind: "query" },
     "todos.count": { kind: "query" },
     "todos.create": { kind: "mutation", idempotent: true },
     "chat.ask": { kind: "stream" },
   });
   ```

   Manual clients then use:

   ```ts
   import { createRpcClient } from "@zeroship/rpc/client";
   import { procedures, type AppRpc } from "./rpc.gen";

   const rpc = createRpcClient<AppRpc>({ procedures });
   ```

3. Explicit manual metadata.
   For small hand-wired clients, users can still pass runtime metadata directly:

   ```ts
   const rpc = createRpcClient<AppRpc>();
   const createTodo = rpc.mutation("todos.create", { idempotent: true });
   ```

The first path is the default Zeroship app experience. The second path gives non-Vite users a clean route without importing server implementation into client bundles. The third path keeps the core client subpath useful without any build integration.

Vite and `zeroship rpc gen` must reject duplicate wire IDs before emitting a client contract. Type-level mapping by ID cannot produce a clear diagnostic when two exports share the same `{ id }`; the build registry owns that error and should include both export names and source paths.

## Client API cleanup

The public `@zeroship/rpc/client` subpath should expose the high-level API:

```ts
export {
  createRpcClient,
  configureRpcClient,
  type InferRpcContract,
  type RpcClient,
  type RpcClientOptions,
  type RpcCallOptions,
  defineRpcProcedures,
  type Query,
  type Mutation,
  type Action,
  type Stream,
  type Subscription,
};
```

`Subscription` remains a descriptor type for server exports and generated metadata. It is intentionally not callable through `RpcClient.call()` or a kind-specific client factory until the subscription client API is designed.

Internal helpers should move behind an internal path or stop being exported from the root:

```ts
// internal only
__callProcedure;
__streamProcedure;
__makeProcedure;
```

The old `client()` builder and new `createRpcClient()` factory should be consolidated before launch. A single factory is easier to teach and avoids two overlapping client concepts.

## Error, retry, and transport behavior

The inferred contract proposal composes with the resilient client redesign:

```ts
const rpc = createRpcClient<AppRpc>({
  baseUrl: "https://example.zeroship.ai",
  headers: async () => ({
    "X-Request-Source": "dashboard",
  }),
  auth: async () => getAccessToken(),
  timeout: 10_000,
  retry: {
    attempts: 3,
    baseDelayMs: 150,
    maxDelayMs: 1_500,
  },
  procedures,
});
```

Retry rules should be kind-aware:

- Queries can retry by default.
- Mutations and actions retry only when `idempotent: true` is known or the caller supplies an idempotency key.
- Streams should not silently retry after partial consumption.
- User-provided `AbortSignal` must cancel timeout and retry loops.
- The client option is named `retryWrites`, not `retryMutations`, because actions are writes for retry-safety purposes.

## Implementation plan

1. Add server procedure brands to the RPC wrapper return types.
2. Add `InferRpcContract` and kind-narrowed client methods to `@zeroship/rpc/client`.
3. Consolidate `client()` and `createRpcClient()` into one public factory.
4. Hide internal `__*` exports from the public client subpath.
5. Update Vite generated stubs to call `createRpcClient` with runtime metadata.
6. Add type tests proving input, output, kind narrowing, and literal ID inference.
7. Add runtime tests proving idempotent metadata affects retry behavior.
8. Document manual non-Vite usage with `import type` and explicit or generated runtime metadata.
9. Keep write retry policy exposed as `retryWrites`.

## Risks

- Wrapper return types must preserve handler inference without making authoring awkward. The helper signatures need type tests around contextual typing.
- `InferRpcContract` depends on importable server module types. Projects with separated frontend and backend packages need exported type entrypoints.
- Runtime metadata can drift if generated files are stale. Vite avoids this. Non-Vite generation should be deterministic and cheap.
- Stream semantics need separate testing because they are not unary JSON calls. Subscription transport can continue to be tested at the lower transport layer until it has a public generic-client API.

## Open questions

None for the core API shape. Remaining work should move into implementation tasks and type tests.
