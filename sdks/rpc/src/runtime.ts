import { type Transformer } from "./encoding.js";
import {
  __makeProcedure,
  type ActionProcedure,
  type MutationProcedure,
  type ProcedureFn,
  type QueryProcedure,
  type StreamProcedure,
} from "./make-procedure.js";
import {
  sendUnary,
  streamCall,
  buildStreamUrl,
  type CallKind,
  type HeaderResolver,
  type RetryConfig,
  type TransportConfig,
} from "./transport.js";
import type {
  ExactRpcProcedureRegistry,
  RegisteredRpcClient,
  RpcClient,
  RpcProcedureRegistry,
} from "./types.js";

export type RuntimeAuthValue =
  | string
  | (() => string | null | undefined | Promise<string | null | undefined>);

export interface RpcRuntimeOptions {
  /** Empty string means same-origin. */
  baseUrl?: string;
  /** Custom fetch implementation. Defaults to globalThis.fetch. */
  fetch?: typeof globalThis.fetch;
  /** Bearer token or resolver. */
  auth?: RuntimeAuthValue;
  /** Default headers added to every generated direct call. */
  headers?: HeaderResolver;
  /** Wire transformer. Defaults to "superjson". */
  transformer?: Transformer;
  /** Default per-attempt timeout in milliseconds. */
  timeout?: number;
  /** Default retry policy for unary direct calls. */
  retry?: RetryConfig;
  /** Global error sink for generated direct calls. */
  onError?: TransportConfig["onError"];
  /** Fires on final UNAUTHENTICATED failures. */
  onAuthExpired?: TransportConfig["onAuthExpired"];
  /** Optional runtime metadata for registry-backed clients. */
  procedures?: Record<string, { kind: CallKind; idempotent?: true }>;
}

export interface RpcCallOptions {
  signal?: AbortSignal;
  headers?: Record<string, string>;
  timeout?: number;
  retry?: RetryConfig;
  idempotencyKey?: string;
}

export interface RpcGeneratedMeta {
  id: string;
  kind: CallKind;
  idempotent?: boolean;
}

export type RpcProcedureOptions = Pick<RpcGeneratedMeta, "idempotent">;

export interface RpcClientFactory {
  procedure<TIn = unknown, TOut = unknown>(
    meta: RpcGeneratedMeta,
  ): ProcedureFn<TIn, TOut>;
  query<TIn = unknown, TOut = unknown>(
    id: string,
    options?: RpcProcedureOptions,
  ): QueryProcedure<TIn, TOut>;
  mutation<TIn = unknown, TOut = unknown>(
    id: string,
    options?: RpcProcedureOptions,
  ): MutationProcedure<TIn, TOut>;
  action<TIn = unknown, TOut = unknown>(
    id: string,
    options?: RpcProcedureOptions,
  ): ActionProcedure<TIn, TOut>;
  stream<TIn = unknown, TOut = unknown>(
    id: string,
    options?: RpcProcedureOptions,
  ): StreamProcedure<TIn, TOut>;
}

interface RuntimeRpcClientFactory extends RpcClientFactory {
  call<TOut = unknown>(
    id: string,
    input?: unknown,
    options?: RpcCallOptions,
  ): Promise<TOut> | AsyncIterableIterator<TOut>;
}

let runtimeOptions: RpcRuntimeOptions = {};

/**
 * Configure generated direct-import RPC stubs.
 *
 * The vite-plugin emits direct calls through this shared runtime, so
 * same-origin defaults stay terse while apps that need custom auth,
 * cross-origin base URLs, timeouts, retry policy, or test fetches can
 * install them once at app boot.
 *
 * Returns a restore function for tests and scoped overrides.
 */
export function configureRpcClient(options: RpcRuntimeOptions): () => void {
  const previous = runtimeOptions;
  runtimeOptions = { ...runtimeOptions, ...options };
  return () => {
    runtimeOptions = previous;
  };
}

export function defineRpcProcedures<Contract = unknown>() {
  return <Procedures extends RpcProcedureRegistry<Contract>>(
    procedures: ExactRpcProcedureRegistry<Contract, Procedures>,
  ): Procedures => procedures;
}

/**
 * Create a procedure factory for manual or generated clients.
 *
 * Non-Vite usage:
 *
 *   const rpc = createRpcClient({ baseUrl, auth });
 *   export const listTodos = rpc.query<Input, Todo[]>("todos.list");
 *   export const saveTodo = rpc.mutation<Input, Todo>("todos.save", {
 *     idempotent: true,
 *   });
 *
 * With no options, the factory reads the global options installed via
 * `configureRpcClient()` at call time. That lets generated Vite stubs
 * stay tiny while still honoring app-level configuration.
 */
export function createRpcClient<Contract>(
  options: RpcRuntimeOptions & {
    procedures: RpcProcedureRegistry<Contract>;
  },
): RegisteredRpcClient<Contract>;
export function createRpcClient(options?: RpcRuntimeOptions): RpcClientFactory;
export function createRpcClient<Contract>(
  options?: RpcRuntimeOptions,
): RpcClient<Contract>;
export function createRpcClient(options?: RpcRuntimeOptions): any {
  const dispatch = <TOut = unknown>(
    id: string,
    kind: CallKind,
    input?: unknown,
    callOptions?: RpcCallOptions,
    meta?: RpcProcedureOptions,
  ): Promise<TOut> | AsyncIterableIterator<TOut> => {
    if (kind === "stream") {
      return streamCall<TOut>(id, input, transportConfig(options), {
        signal: callOptions?.signal,
        headers: callOptions?.headers,
        timeout: callOptions?.timeout,
      });
    }
    return sendUnary<TOut>(id, input, transportConfig(options), {
      kind,
      idempotent: meta?.idempotent,
      idempotencyKey: callOptions?.idempotencyKey,
      signal: callOptions?.signal,
      headers: callOptions?.headers,
      timeout: callOptions?.timeout,
      retry: callOptions?.retry,
    });
  };

  const make = <TIn, TOut>(meta: RpcGeneratedMeta): ProcedureFn<TIn, TOut> =>
    __makeProcedure<TIn, TOut>(
      (input, callOptions) => {
        return dispatch<TOut>(meta.id, meta.kind, input, callOptions, {
          idempotent: meta.idempotent,
        });
      },
      meta,
    );

  return {
    call<TOut = unknown>(
      id: string,
      input?: unknown,
      callOptions?: RpcCallOptions,
    ): Promise<TOut> | AsyncIterableIterator<TOut> {
      const meta = options?.procedures?.[id] ?? runtimeOptions.procedures?.[id];
      if (!meta) {
        throw new Error(
          `[zeroship/rpc] rpc.call("${id}") requires createRpcClient({ procedures }) metadata. ` +
            `Use rpc.query("${id}") / rpc.mutation("${id}") / rpc.action("${id}") / rpc.stream("${id}") when no registry is available.`,
        );
      }
      return dispatch<TOut>(id, meta.kind, input, callOptions, meta);
    },
    procedure: make,
    query<TIn = unknown, TOut = unknown>(
      id: string,
      procedureOptions?: RpcProcedureOptions,
    ): QueryProcedure<TIn, TOut> {
      return make<TIn, TOut>({
        id,
        kind: "query",
        ...procedureOptions,
      }) as QueryProcedure<TIn, TOut>;
    },
    mutation<TIn = unknown, TOut = unknown>(
      id: string,
      procedureOptions?: RpcProcedureOptions,
    ): MutationProcedure<TIn, TOut> {
      return make<TIn, TOut>({
        id,
        kind: "mutation",
        ...procedureOptions,
      }) as MutationProcedure<TIn, TOut>;
    },
    action<TIn = unknown, TOut = unknown>(
      id: string,
      procedureOptions?: RpcProcedureOptions,
    ): ActionProcedure<TIn, TOut> {
      return make<TIn, TOut>({
        id,
        kind: "action",
        ...procedureOptions,
      }) as ActionProcedure<TIn, TOut>;
    },
    stream<TIn = unknown, TOut = unknown>(
      id: string,
      procedureOptions?: RpcProcedureOptions,
    ): StreamProcedure<TIn, TOut> {
      const proc = make<TIn, TOut>({
        id,
        kind: "stream",
        ...procedureOptions,
      }) as StreamProcedure<TIn, TOut> & {
        streamUrl(input?: TIn): string | Promise<string>;
      };
      Object.defineProperty(proc, "streamUrl", {
        value: (input?: TIn) => buildStreamUrl(id, input, transportConfig(options)),
        enumerable: true,
        configurable: true,
        writable: false,
      });
      return proc;
    },
  };
}

export function __callProcedure<TOut = unknown>(
  id: string,
  kind: CallKind,
  input: unknown,
  options?: RpcCallOptions,
  meta?: Pick<RpcGeneratedMeta, "idempotent">,
): Promise<TOut> {
  if (kind === "stream") {
    return streamCall<TOut>(id, input, transportConfig(), {
      signal: options?.signal,
      headers: options?.headers,
      timeout: options?.timeout,
    }) as unknown as Promise<TOut>;
  }
  return sendUnary<TOut>(id, input, transportConfig(), {
    kind,
    idempotent: meta?.idempotent,
    idempotencyKey: options?.idempotencyKey,
    signal: options?.signal,
    headers: options?.headers,
    timeout: options?.timeout,
    retry: options?.retry,
  });
}

export function __streamProcedure<TOut = unknown>(
  id: string,
  input: unknown,
  options?: RpcCallOptions,
): AsyncIterableIterator<TOut> {
  return streamCall<TOut>(id, input, transportConfig(), {
    signal: options?.signal,
    headers: options?.headers,
    timeout: options?.timeout,
  });
}

function transportConfig(localOptions?: RpcRuntimeOptions): TransportConfig {
  const merged: RpcRuntimeOptions = {
    ...runtimeOptions,
    ...(localOptions ?? {}),
  };
  const fetchFn = merged.fetch ?? globalThis.fetch?.bind(globalThis);
  if (!fetchFn) {
    throw new Error(
      "[zeroship/rpc] no fetch implementation — pass `fetch` to configureRpcClient({}) or run on a runtime that exposes globalThis.fetch.",
    );
  }

  const authResolver: TransportConfig["authResolver"] =
    typeof merged.auth === "function"
      ? merged.auth
      : merged.auth !== undefined
        ? () => merged.auth as string
        : () => null;

  return {
    baseUrl: merged.baseUrl ?? "",
    fetch: fetchFn,
    transformer: merged.transformer ?? "superjson",
    authResolver,
    headersResolver: normalizeHeadersResolver(merged.headers),
    timeout: merged.timeout,
    retry: merged.retry,
    onError: merged.onError,
    onAuthExpired: merged.onAuthExpired,
  };
}

function normalizeHeadersResolver(
  headers: HeaderResolver | undefined,
): TransportConfig["headersResolver"] {
  if (!headers) return undefined;
  return typeof headers === "function" ? headers : () => headers;
}
