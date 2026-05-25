//
// Public surface of `@zeroship/rpc-client`:
//
//   - `client<App>(opts)` — the typed client builder
//   - `RpcError` + `ErrorCode` — structured failure model
//   - `typeMarker<T>()` — phantom-type helper for the App-type fallback
//
// WebSocket subscriptions are not exported here yet. Calling
// `.subscribe()` on a procedure handle throws `UNIMPLEMENTED` today.

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

// `__makeProcedure`.
//
// `__makeProcedure(call, meta)` wraps a raw HTTP-RPC closure in a
// callable procedure reference. The vite-plugin's client transform
// emits one per server export; user code imports them as plain async
// functions:
//
//   import { list, add } from "../server/todos";
//   const todos = await list({ limit: 50 });   // direct call
export { __makeProcedure, __SERVER_REFERENCE } from "./make-procedure.js";
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
