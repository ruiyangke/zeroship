// Public client surface for `@zeroship/rpc/client`.

export {
  createRpcClient,
  configureRpcClient,
  defineRpcProcedures,
} from "./runtime.js";
export type {
  RpcClientFactory,
  RpcRuntimeOptions,
  RpcCallOptions,
  RpcGeneratedMeta,
  RpcProcedureOptions,
} from "./runtime.js";

export { RpcError, ErrorCode, isRpcError, parseErrorResponse } from "./error.js";
export type { ErrorCode as ErrorCodeType, RpcErrorInit } from "./error.js";

export type { Transformer } from "./encoding.js";
export type { RetryConfig, RetryOptions } from "./transport.js";

export { newUuidV7 } from "./idempotency.js";
export type {
  ProcedureKind,
  ProcedureBuildMeta,
  ProcedureCallOptions,
  ProcedureCaller,
  ProcedureFn,
  QueryProcedure,
  MutationProcedure,
  ActionProcedure,
  StreamProcedure,
  SubscriptionProcedure,
} from "./make-procedure.js";

export type {
  InferRpcContract,
  RpcKind,
  ServerProcedure,
  RpcClient,
  RegisteredRpcClient,
  RpcProcedureRegistry,
  Query,
  Mutation,
  Action,
  Stream,
  Subscription,
} from "./types.js";
