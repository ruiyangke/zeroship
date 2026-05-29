// Shared type vocabulary for `@zeroship/rpc`.
//
// This module is intentionally type-first and runtime-empty. The client
// and server subpaths both import these types without depending on each
// other's runtime code.

export type RpcKind = "query" | "mutation" | "action" | "stream" | "subscription";

export interface ProcedureSchema<T = unknown> {
  parse(input: unknown): T;
}

export type AuthLevel = "anon" | "user" | "admin";
export type RateLimitScope = "ip" | "user" | "session" | "app";

export interface RateLimit {
  rpm?: number;
  rps?: number;
  per?: RateLimitScope;
}

export interface Timeout {
  ms?: number;
}

export interface ProcedureConfig<TIn = unknown, TOut = unknown> {
  /** Stable wire id. Required for production deploys and inferred contracts. */
  id?: string;
  /** Override the wrapper-inferred procedure kind. */
  kind?: RpcKind;
  /** Writes can opt into idempotency-key dedupe. */
  idempotent?: boolean;
  /** Idempotency-key TTL override. Default 24 h; max 7 d. */
  idempotencyTtl?: { hours?: number };
  auth?: AuthLevel;
  rateLimit?: RateLimit;
  maxInputBytes?: number;
  timeout?: Timeout;
  middleware?: string[];
  /** Build-time cold-start hint; lazy procedures dynamic-import on first call. */
  lazy?: boolean;
  input?: ProcedureSchema<TIn>;
  output?: ProcedureSchema<TOut>;
}

declare const rpcKind: unique symbol;
declare const rpcId: unique symbol;
declare const rpcInput: unique symbol;
declare const rpcOutput: unique symbol;
declare const rpcMeta: unique symbol;

export type NoClientRpcMeta = {
  readonly __clientRpcMeta?: never;
};

export type ClientRpcMeta =
  | NoClientRpcMeta
  | {
      idempotent?: true;
    };

export type ServerProcedure<
  Kind extends RpcKind = RpcKind,
  Input = unknown,
  Output = unknown,
  Id extends string = string,
  Meta extends ClientRpcMeta = NoClientRpcMeta,
> = {
  (input: Input): Kind extends "stream" | "subscription"
    ? AsyncIterable<Output>
    : Output | Promise<Output>;
  config?: ProcedureConfig<Input, Output> & { id?: Id; kind?: Kind } & Meta;
  readonly [rpcKind]?: Kind;
  readonly [rpcId]?: Id;
  readonly [rpcInput]?: Input;
  readonly [rpcOutput]?: Output;
  readonly [rpcMeta]?: Meta;
};

export type ResponseStreamProcedure<
  Input = unknown,
  Id extends string = string,
  Meta extends ClientRpcMeta = NoClientRpcMeta,
> = {
  (input: Input): Response | Promise<Response>;
  config?: ProcedureConfig<Input, never> & { id?: Id; kind?: "stream" } & Meta;
  readonly [rpcKind]?: "stream";
  readonly [rpcId]?: Id;
  readonly [rpcInput]?: Input;
  readonly [rpcOutput]?: never;
  readonly [rpcMeta]?: Meta;
};

export type Query<
  Input = void,
  Output = unknown,
  Meta extends ClientRpcMeta = NoClientRpcMeta,
> = RpcDescriptor<"query", Input, Output, Meta>;

export type Mutation<
  Input = void,
  Output = unknown,
  Meta extends ClientRpcMeta = NoClientRpcMeta,
> = RpcDescriptor<"mutation", Input, Output, Meta>;

export type Action<
  Input = void,
  Output = unknown,
  Meta extends ClientRpcMeta = NoClientRpcMeta,
> = RpcDescriptor<"action", Input, Output, Meta>;

export type Stream<
  Input = void,
  Output = unknown,
  Meta extends ClientRpcMeta = NoClientRpcMeta,
> = RpcDescriptor<"stream", Input, Output, Meta>;

export type Subscription<
  Input = void,
  Output = unknown,
  Meta extends ClientRpcMeta = NoClientRpcMeta,
> = RpcDescriptor<"subscription", Input, Output, Meta>;

export interface RpcDescriptor<
  Kind extends RpcKind = RpcKind,
  Input = unknown,
  Output = unknown,
  Meta extends ClientRpcMeta = NoClientRpcMeta,
> {
  kind: Kind;
  input: Input;
  output: Output;
  meta?: Meta;
}

type InferredProcedureId<Id> = Id extends string
  ? string extends Id
    ? never
    : Id
  : never;

export type InferRpcContract<TProcedures> = {
  [K in keyof TProcedures as TProcedures[K] extends ServerProcedure<
      any,
      any,
      any,
      infer Id,
      any
    >
      ? InferredProcedureId<Id>
      : TProcedures[K] extends ResponseStreamProcedure<any, infer Id, any>
        ? InferredProcedureId<Id>
        : never]: TProcedures[K] extends ServerProcedure<
      infer Kind,
      infer Input,
      infer Output,
      infer _Id,
      infer Meta
    >
      ? RpcDescriptor<Kind, Input, Output, Meta>
      : TProcedures[K] extends ResponseStreamProcedure<
            infer Input,
            infer _Id,
            infer Meta
          >
        ? RpcDescriptor<"stream", Input, never, Meta>
        : never;
};

type DescriptorKind<T> = T extends RpcDescriptor<infer Kind, any, any, any>
  ? Kind
  : never;

type DescriptorInput<T> = T extends RpcDescriptor<any, infer Input, any, any>
  ? Input
  : unknown;

type DescriptorOutput<T> = T extends RpcDescriptor<any, any, infer Output, any>
  ? Output
  : unknown;

type DescriptorMeta<T> = T extends RpcDescriptor<any, any, any, infer Meta>
  ? Meta
  : NoClientRpcMeta;

export type RpcIdsByKind<Contract, Kind extends RpcKind> = {
  [K in keyof Contract & string]: DescriptorKind<Contract[K]> extends Kind ? K : never;
}[keyof Contract & string];

type IsUnknown<T> = unknown extends T ? ([T] extends [unknown] ? true : false) : false;

type RpcId<Contract, Kind extends RpcKind> = IsUnknown<Contract> extends true
  ? string
  : RpcIdsByKind<Contract, Kind> & string;

type ContractDescriptor<Contract, Id extends string> = IsUnknown<Contract> extends true
  ? unknown
  : Id extends keyof Contract
    ? Contract[Id]
    : unknown;

type RpcCallableId<Contract> = IsUnknown<Contract> extends true
  ? string
  : {
      [K in keyof Contract & string]: DescriptorKind<Contract[K]> extends "subscription"
        ? never
        : K;
    }[keyof Contract & string];

export type RpcProcedureRuntimeMeta<TDescriptor> = IsUnknown<TDescriptor> extends true
  ? { kind: RpcKind; idempotent?: true }
  : DescriptorMeta<TDescriptor> extends { idempotent?: true }
    ? { kind: DescriptorKind<TDescriptor>; idempotent: true }
    : { kind: DescriptorKind<TDescriptor>; idempotent?: never };

export type RpcProcedureRegistry<Contract> = IsUnknown<Contract> extends true
  ? Record<string, { kind: RpcKind; idempotent?: true }>
  : {
      [K in keyof Contract & string]: RpcProcedureRuntimeMeta<Contract[K]>;
    };

export type ExactRpcProcedureRegistry<
  Contract,
  Procedures extends RpcProcedureRegistry<Contract>,
> = Procedures & Record<Exclude<keyof Procedures, keyof RpcProcedureRegistry<Contract>>, never>;

export type RpcProcedureOptions<TDescriptor = unknown> = IsUnknown<TDescriptor> extends true
  ? { idempotent?: true }
  : DescriptorMeta<TDescriptor> extends { idempotent?: true }
    ? { idempotent?: true }
    : { idempotent?: never };

type ProcedureArgs<Input> = [Input] extends [void]
  ? [input?: undefined, options?: RpcCallOptions]
  : [input: Input, options?: RpcCallOptions];

export type RpcClientProcedure<TDescriptor> =
  DescriptorKind<TDescriptor> extends "subscription"
    ? never
    : (
        ...args: ProcedureArgs<DescriptorInput<TDescriptor>>
      ) => DescriptorKind<TDescriptor> extends "stream"
        ? AsyncIterableIterator<DescriptorOutput<TDescriptor>>
        : Promise<DescriptorOutput<TDescriptor>>;

export type RpcStreamProcedure<TDescriptor> = RpcClientProcedure<TDescriptor> & {
  streamUrl(
    input?: DescriptorInput<TDescriptor>,
  ): string | Promise<string>;
};

export interface RpcCallOptions {
  signal?: AbortSignal;
  headers?: Record<string, string>;
  timeout?: number;
  retry?: unknown;
  idempotencyKey?: string;
}

export interface RpcClient<Contract = unknown> {
  query<Id extends RpcId<Contract, "query">>(
    id: Id,
    options?: RpcProcedureOptions<ContractDescriptor<Contract, Id>>,
  ): RpcClientProcedure<ContractDescriptor<Contract, Id>>;
  mutation<Id extends RpcId<Contract, "mutation">>(
    id: Id,
    options?: RpcProcedureOptions<ContractDescriptor<Contract, Id>>,
  ): RpcClientProcedure<ContractDescriptor<Contract, Id>>;
  action<Id extends RpcId<Contract, "action">>(
    id: Id,
    options?: RpcProcedureOptions<ContractDescriptor<Contract, Id>>,
  ): RpcClientProcedure<ContractDescriptor<Contract, Id>>;
  stream<Id extends RpcId<Contract, "stream">>(
    id: Id,
    options?: RpcProcedureOptions<ContractDescriptor<Contract, Id>>,
  ): RpcStreamProcedure<ContractDescriptor<Contract, Id>>;
}

export interface RegisteredRpcClient<Contract = unknown> extends RpcClient<Contract> {
  call<Id extends RpcCallableId<Contract>>(
    id: Id,
    ...args: ProcedureArgs<DescriptorInput<ContractDescriptor<Contract, Id>>>
  ): DescriptorKind<ContractDescriptor<Contract, Id>> extends "stream"
    ? AsyncIterableIterator<DescriptorOutput<ContractDescriptor<Contract, Id>>>
    : Promise<DescriptorOutput<ContractDescriptor<Contract, Id>>>;
}
