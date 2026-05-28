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
  ResponseStreamProcedure,
  RpcKind,
  ServerProcedure,
} from "./types";

export type {
  ProcedureConfig,
  ResponseStreamProcedure,
  RpcKind,
  ServerProcedure,
} from "./types";

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
type StreamItem<Output> =
  Awaited<Output> extends AsyncIterable<infer Item> ? Item : never;

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
  Output,
  const Config extends ProcedureConfig<void, Awaited<Output>> & {
    kind: "query" | "mutation" | "action";
  },
>(
  handler: () => MaybePromise<Output>,
  config: Config,
): ServerProcedure<
  ConfigKind<Config>,
  void,
  Awaited<Output>,
  ConfigId<Config>,
  ConfigMeta<Config>
>;
export function procedure<
  Input,
  Output,
  const Config extends ProcedureConfig<Input, Awaited<Output>> & {
    kind: "query" | "mutation" | "action";
  },
>(
  handler: (input: Input) => MaybePromise<Output>,
  config: Config,
): ServerProcedure<
  ConfigKind<Config>,
  Input,
  Awaited<Output>,
  ConfigId<Config>,
  ConfigMeta<Config>
>;
export function procedure<
  Output extends AsyncIterable<unknown>,
  const Config extends ProcedureConfig<void, StreamItem<Output>> & {
    kind: "stream" | "subscription";
  },
>(
  handler: () => MaybePromise<Output>,
  config: Config,
): ServerProcedure<
  ConfigKind<Config>,
  void,
  StreamItem<Output>,
  ConfigId<Config>,
  ConfigMeta<Config>
>;
export function procedure<
  Input,
  Output extends AsyncIterable<unknown>,
  const Config extends ProcedureConfig<Input, StreamItem<Output>> & {
    kind: "stream" | "subscription";
  },
>(
  handler: (input: Input) => MaybePromise<Output>,
  config: Config,
): ServerProcedure<
  ConfigKind<Config>,
  Input,
  StreamItem<Output>,
  ConfigId<Config>,
  ConfigMeta<Config>
>;
export function procedure<H extends Handler>(handler: H, config?: ProcedureConfig): H;
export function procedure<H extends Handler>(handler: H, config?: ProcedureConfig): H {
  return attach(handler, "procedure", config);
}

export function query<
  Output,
  const Config extends ProcedureConfig<void, Awaited<Output>> = ProcedureConfig<
    void,
    Awaited<Output>
  >,
>(
  handler: () => MaybePromise<Output>,
  config?: Config,
): ServerProcedure<
  "query",
  void,
  Awaited<Output>,
  ConfigId<Config>,
  ConfigMeta<Config>
>;
export function query<
  Input,
  Output,
  const Config extends ProcedureConfig<Input, Awaited<Output>> = ProcedureConfig<
    Input,
    Awaited<Output>
  >,
>(
  handler: (input: Input) => MaybePromise<Output>,
  config?: Config,
): ServerProcedure<
  "query",
  Input,
  Awaited<Output>,
  ConfigId<Config>,
  ConfigMeta<Config>
>;
export function query(handler: Handler, config?: ProcedureConfig): any {
  return attach(handler, "query", config);
}

export function mutation<
  Output,
  const Config extends ProcedureConfig<void, Awaited<Output>> = ProcedureConfig<
    void,
    Awaited<Output>
  >,
>(
  handler: () => MaybePromise<Output>,
  config?: Config,
): ServerProcedure<
  "mutation",
  void,
  Awaited<Output>,
  ConfigId<Config>,
  ConfigMeta<Config>
>;
export function mutation<
  Input,
  Output,
  const Config extends ProcedureConfig<Input, Awaited<Output>> = ProcedureConfig<
    Input,
    Awaited<Output>
  >,
>(
  handler: (input: Input) => MaybePromise<Output>,
  config?: Config,
): ServerProcedure<
  "mutation",
  Input,
  Awaited<Output>,
  ConfigId<Config>,
  ConfigMeta<Config>
>;
export function mutation(handler: Handler, config?: ProcedureConfig): any {
  return attach(handler, "mutation", config);
}

export function action<
  Output,
  const Config extends ProcedureConfig<void, Awaited<Output>> = ProcedureConfig<
    void,
    Awaited<Output>
  >,
>(
  handler: () => MaybePromise<Output>,
  config?: Config,
): ServerProcedure<
  "action",
  void,
  Awaited<Output>,
  ConfigId<Config>,
  ConfigMeta<Config>
>;
export function action<
  Input,
  Output,
  const Config extends ProcedureConfig<Input, Awaited<Output>> = ProcedureConfig<
    Input,
    Awaited<Output>
  >,
>(
  handler: (input: Input) => MaybePromise<Output>,
  config?: Config,
): ServerProcedure<
  "action",
  Input,
  Awaited<Output>,
  ConfigId<Config>,
  ConfigMeta<Config>
>;
export function action(handler: Handler, config?: ProcedureConfig): any {
  return attach(handler, "action", config);
}

export function stream<
  Output extends AsyncIterable<unknown>,
  const Config extends ProcedureConfig<void, StreamItem<Output>> = ProcedureConfig<
    void,
    StreamItem<Output>
  >,
>(
  handler: () => MaybePromise<Output>,
  config?: Config,
): ServerProcedure<
  "stream",
  void,
  StreamItem<Output>,
  ConfigId<Config>,
  ConfigMeta<Config>
>;
export function stream<
  Input,
  Output extends AsyncIterable<unknown>,
  const Config extends ProcedureConfig<Input, StreamItem<Output>> = ProcedureConfig<
    Input,
    StreamItem<Output>
  >,
>(
  handler: (input: Input) => MaybePromise<Output>,
  config?: Config,
): ServerProcedure<
  "stream",
  Input,
  StreamItem<Output>,
  ConfigId<Config>,
  ConfigMeta<Config>
>;
export function stream(handler: Handler, config?: ProcedureConfig): any {
  return attach(handler, "stream", config);
}

export function streamResponse<
  const Config extends ProcedureConfig<void, never> = ProcedureConfig<void, never>,
>(
  handler: () => MaybePromise<Response>,
  config?: Config,
): ResponseStreamProcedure<void, ConfigId<Config>, ConfigMeta<Config>>;
export function streamResponse<
  Input,
  const Config extends ProcedureConfig<Input, never> = ProcedureConfig<Input, never>,
>(
  handler: (input: Input) => MaybePromise<Response>,
  config?: Config,
): ResponseStreamProcedure<Input, ConfigId<Config>, ConfigMeta<Config>>;
export function streamResponse(handler: Handler, config?: ProcedureConfig): any {
  return attach(handler, "stream", config);
}

export function subscription<
  Output extends AsyncIterable<unknown>,
  const Config extends ProcedureConfig<void, StreamItem<Output>> = ProcedureConfig<
    void,
    StreamItem<Output>
  >,
>(
  handler: () => MaybePromise<Output>,
  config?: Config,
): ServerProcedure<
  "subscription",
  void,
  StreamItem<Output>,
  ConfigId<Config>,
  ConfigMeta<Config>
>;
export function subscription<
  Input,
  Output extends AsyncIterable<unknown>,
  const Config extends ProcedureConfig<Input, StreamItem<Output>> = ProcedureConfig<
    Input,
    StreamItem<Output>
  >,
>(
  handler: (input: Input) => MaybePromise<Output>,
  config?: Config,
): ServerProcedure<
  "subscription",
  Input,
  StreamItem<Output>,
  ConfigId<Config>,
  ConfigMeta<Config>
>;
export function subscription(handler: Handler, config?: ProcedureConfig): any {
  return attach(handler, "subscription", config);
}

export type {
  ClientRpcMeta,
  NoClientRpcMeta,
};
