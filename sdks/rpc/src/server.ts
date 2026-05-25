// Server-side RPC wrappers for `@zeroship/rpc/server`.
//
// Wrappers are identity functions at runtime. They tag handlers with
// `.config` and `__zsKind` so the Vite plugin and runtime dispatcher can
// recognize procedures, while TypeScript keeps erased metadata for
// `InferRpcContract`.

import type {
  ClientRpcMeta,
  NoClientRpcMeta,
  ProcedureConfig,
  RpcKind,
  ServerProcedure,
} from "./types.js";

export type { ProcedureConfig, RpcKind, ServerProcedure } from "./types.js";

type Handler = (...args: any[]) => unknown;
type WrapperMarker =
  | "procedure"
  | "query"
  | "mutation"
  | "action"
  | "stream"
  | "subscription";

type ConfigMeta<Config> = Config extends { idempotent: true }
  ? { idempotent?: true }
  : NoClientRpcMeta;

type ConfigId<Config> = Config extends { id: infer Id extends string }
  ? Id
  : string;
type ConfigKind<Config> = Config extends { kind: infer Kind extends RpcKind }
  ? Kind
  : never;

type MaybePromise<T> = T | Promise<T>;
type AnyUnaryHandler =
  | (() => MaybePromise<unknown>)
  | ((input: any) => MaybePromise<unknown>);
type AnyStreamHandler =
  | (() => MaybePromise<AsyncIterable<unknown>>)
  | ((input: any) => MaybePromise<AsyncIterable<unknown>>);
type HandlerInput<H extends (...args: any[]) => unknown> = Parameters<H> extends []
  ? void
  : Parameters<H>[0];
type UnaryOutput<H extends (...args: any[]) => unknown> = Awaited<ReturnType<H>>;
type StreamOutput<H extends (...args: any[]) => unknown> =
  Awaited<ReturnType<H>> extends AsyncIterable<infer Item> ? Item : never;

function attach<H extends Handler>(
  handler: H,
  marker: WrapperMarker,
  config?: ProcedureConfig,
): H {
  try {
    const baseConfig: ProcedureConfig | undefined = config;
    if (marker !== "procedure") {
      const merged: ProcedureConfig =
        baseConfig === undefined
          ? { kind: marker }
          : baseConfig.kind === undefined
            ? { ...baseConfig, kind: marker }
            : baseConfig;
      Object.defineProperty(handler, "config", {
        value: merged,
        enumerable: true,
        configurable: true,
        writable: true,
      });
    } else if (baseConfig !== undefined) {
      Object.defineProperty(handler, "config", {
        value: baseConfig,
        enumerable: true,
        configurable: true,
        writable: true,
      });
    }
    Object.defineProperty(handler, "__zsKind", {
      value: marker,
      enumerable: false,
      configurable: true,
      writable: true,
    });
  } catch {
    /* frozen function: keep identity semantics */
  }
  return handler;
}

export function procedure<
  const H extends AnyUnaryHandler,
  const Config extends ProcedureConfig<HandlerInput<H>, UnaryOutput<H>> & {
    kind: "query" | "mutation" | "action";
  },
>(
  handler: H,
  config: Config,
): ServerProcedure<
  ConfigKind<Config>,
  HandlerInput<H>,
  UnaryOutput<H>,
  ConfigId<Config>,
  ConfigMeta<Config>
>;
export function procedure<
  const H extends AnyStreamHandler,
  const Config extends ProcedureConfig<HandlerInput<H>, StreamOutput<H>> & {
    kind: "stream" | "subscription";
  },
>(
  handler: H,
  config: Config,
): ServerProcedure<
  ConfigKind<Config>,
  HandlerInput<H>,
  StreamOutput<H>,
  ConfigId<Config>,
  ConfigMeta<Config>
>;
export function procedure<H extends Handler>(handler: H, config?: ProcedureConfig): H;
export function procedure<H extends Handler>(handler: H, config?: ProcedureConfig): H {
  return attach(handler, "procedure", config);
}

export function query<
  const H extends AnyUnaryHandler,
  const Config extends ProcedureConfig<HandlerInput<H>, UnaryOutput<H>> = ProcedureConfig<
    HandlerInput<H>,
    UnaryOutput<H>
  >,
>(
  handler: H,
  config?: Config,
): ServerProcedure<
  "query",
  HandlerInput<H>,
  UnaryOutput<H>,
  ConfigId<Config>,
  ConfigMeta<Config>
> {
  return attach(handler as unknown as Handler, "query", config) as ServerProcedure<
    "query",
    HandlerInput<H>,
    UnaryOutput<H>,
    ConfigId<Config>,
    ConfigMeta<Config>
  >;
}

export function mutation<
  const H extends AnyUnaryHandler,
  const Config extends ProcedureConfig<HandlerInput<H>, UnaryOutput<H>> = ProcedureConfig<
    HandlerInput<H>,
    UnaryOutput<H>
  >,
>(
  handler: H,
  config?: Config,
): ServerProcedure<
  "mutation",
  HandlerInput<H>,
  UnaryOutput<H>,
  ConfigId<Config>,
  ConfigMeta<Config>
> {
  return attach(handler as unknown as Handler, "mutation", config) as ServerProcedure<
    "mutation",
    HandlerInput<H>,
    UnaryOutput<H>,
    ConfigId<Config>,
    ConfigMeta<Config>
  >;
}

export function action<
  const H extends AnyUnaryHandler,
  const Config extends ProcedureConfig<HandlerInput<H>, UnaryOutput<H>> = ProcedureConfig<
    HandlerInput<H>,
    UnaryOutput<H>
  >,
>(
  handler: H,
  config?: Config,
): ServerProcedure<
  "action",
  HandlerInput<H>,
  UnaryOutput<H>,
  ConfigId<Config>,
  ConfigMeta<Config>
> {
  return attach(handler as unknown as Handler, "action", config) as ServerProcedure<
    "action",
    HandlerInput<H>,
    UnaryOutput<H>,
    ConfigId<Config>,
    ConfigMeta<Config>
  >;
}

export function stream<
  const H extends AnyStreamHandler,
  const Config extends ProcedureConfig<HandlerInput<H>, StreamOutput<H>> = ProcedureConfig<
    HandlerInput<H>,
    StreamOutput<H>
  >,
>(
  handler: H,
  config?: Config,
): ServerProcedure<
  "stream",
  HandlerInput<H>,
  StreamOutput<H>,
  ConfigId<Config>,
  ConfigMeta<Config>
> {
  return attach(handler as unknown as Handler, "stream", config) as ServerProcedure<
    "stream",
    HandlerInput<H>,
    StreamOutput<H>,
    ConfigId<Config>,
    ConfigMeta<Config>
  >;
}

export function subscription<
  const H extends AnyStreamHandler,
  const Config extends ProcedureConfig<HandlerInput<H>, StreamOutput<H>> = ProcedureConfig<
    HandlerInput<H>,
    StreamOutput<H>
  >,
>(
  handler: H,
  config?: Config,
): ServerProcedure<
  "subscription",
  HandlerInput<H>,
  StreamOutput<H>,
  ConfigId<Config>,
  ConfigMeta<Config>
> {
  return attach(handler as unknown as Handler, "subscription", config) as ServerProcedure<
    "subscription",
    HandlerInput<H>,
    StreamOutput<H>,
    ConfigId<Config>,
    ConfigMeta<Config>
  >;
}

export type {
  ClientRpcMeta,
  NoClientRpcMeta,
};
