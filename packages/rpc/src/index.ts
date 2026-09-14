// Public client surface for `@zeroship/rpc/client`.

export {
  createRpcClient,
  configureRpcClient,
  defineRpcProcedures,
} from "./runtime";
export type {
  RpcClientFactory,
  RpcRuntimeOptions,
  RpcCallOptions,
  RpcGeneratedMeta,
  RpcProcedureOptions,
} from "./runtime";

export { RpcError, ErrorCode, isRpcError, parseErrorResponse } from "./error";
export type { ErrorCode as ErrorCodeType, RpcErrorInit } from "./error";

export type { Transformer } from "./encoding";
export type { RetryConfig, RetryOptions } from "./transport";

export { newUuidV7 } from "./idempotency";
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
} from "./make-procedure";

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
} from "./types";
