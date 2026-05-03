//
// Public surface of `@zeroship/rpc-client`. Phase 3 ships:
//
//   - `client<App>(opts)` — the typed client builder
//   - `RpcError` + `ErrorCode` — structured failure model
//   - `typeMarker<T>()` — phantom-type helper for the App-type fallback
//
// Streams (Phase 4), React Query bindings (Phase 5), subscriptions
// (Phase 8) are NOT exported. Calling `.stream()` / `.subscribe()` on
// any procedure handle throws UNIMPLEMENTED today.

export { client } from "./client.js";
export type {
  ClientOptions,
  CallOptions,
  FullCallOptions,
  ProcedureMeta,
  ProcedureType,
  ProcedureHandle,
  TypedClient,
  AuthValue,
} from "./client.js";

export { RpcError, ErrorCode, isRpcError, parseErrorResponse } from "./error.js";
export type { ErrorCode as ErrorCodeType, RpcErrorInit } from "./error.js";

export type { Transformer } from "./encoding.js";

export { newUuidV7 } from "./idempotency.js";

// Phase 5 — `__makeProcedure` and the closure-private hook registry.
//
// `__makeProcedure(call, meta)` wraps a raw HTTP-RPC closure in a
// callable + hooks-on-function object. The vite-plugin's client
// transform emits one per server export; user code imports them as
// plain async functions:
//
//   import { list, add } from "../server/todos";
//   const todos = await list({ limit: 50 });   // direct call
//   list.useQuery({ limit: 50 });              // React (when @zeroship/rpc-react is loaded)
//
// Hooks are attached via getters that read from `_hookRegistry`, a
// closure-private object populated as a side effect of importing
// `@zeroship/rpc-react`. Frameworks other than React (Vue, Solid,
// vanilla) use the same package — React Query never enters the bundle
// until `@zeroship/rpc-react` is imported.
//
// The registry is also exposed on the public subpath
// `@zeroship/rpc-client/_hooks` so the React adapter can populate it
// without static-importing into core internals.
export { __makeProcedure } from "./make-procedure.js";
export type {
  ProcedureKind,
  ProcedureBuildMeta,
  ProcedureCaller,
  ProcedureFn,
  QueryProcedure,
  MutationProcedure,
  StreamProcedure,
  SubscriptionProcedure,
} from "./make-procedure.js";

export { _hookRegistry, HOOK_UNAVAILABLE_MESSAGE } from "./_hooks.js";
export type { HookRegistry } from "./_hooks.js";

/**
 * Phantom helper. Used purely to attach a function signature to a
 * procedure id when declaring an `App` type without runtime metadata.
 *
 *   type App = {
 *     listTodos: ProcedureType<"query", { limit?: number }, Todo[]>;
 *     // ...or via typeMarker for a more terse signature attachment:
 *     legacy: ReturnType<typeof typeMarker<(arg: number) => Promise<string>>>;
 *   };
 *
 * The function returns `undefined`; the only thing that matters is the
 * compile-time type. Convex uses an analogous pattern for its API
 * declaration.
 */
export function typeMarker<_T>(): _T extends never ? never : undefined {
  return undefined as _T extends never ? never : undefined;
}
