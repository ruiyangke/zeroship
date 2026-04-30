// packages/zeroship-rpc-client/src/index.ts
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
