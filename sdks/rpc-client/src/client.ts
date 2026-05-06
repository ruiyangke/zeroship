//
// `client<App>(opts)` — typed client builder. Three usage modes:
//
//   1. Escape hatch:
//        rpc.call("listTodos", { limit: 50 }, { kind: "query" })
//      Always available, untyped on the input/output sides.
//
//   2. Proxy with `procedures` registry:
//        client({ procedures: { listTodos: { kind: "query" } } })
//        rpc.listTodos.query({ limit: 50 })
//      Explicit declaration drives both runtime kind dispatch and
//      typeMarker-style typing.
//
//   3. Type-only via the `App` generic:
//        client<App>({ ... })
//        rpc.listTodos.query({ limit: 50 })
//      Same proxy at runtime; the App type provides input/output types.
//      Procedure kind is auto-inferred at runtime per the spec rule
//      "names that look like queries are queries; everything else is a
//      mutation". Callers that need precise kind override should use
//      mode #2 OR call rpc.<id>.query / rpc.<id>.mutation explicitly.
//
// The proxy unwraps dotted ids — `rpc.todos.list.query()` and
// `rpc["todos.list"].query()` route to the same wire id "todos.list".

import {
  sendUnary,
  streamCall,
  subscribeCall,
  buildStreamUrl,
  type CallKind,
  type SubscribeOptions,
  type SubscriptionHandle,
  type TransportConfig,
} from "./transport.js";
import { createBatchLink, type BatchLink } from "./batch.js";
import { type Transformer } from "./encoding.js";
import { RpcError } from "./error.js";

// ── Public option types ────────────────────────────────────────────────

/** Function or static value that yields the bearer token. */
export type AuthValue =
  | string
  | (() => string | null | undefined | Promise<string | null | undefined>);

/**
 * Optional per-procedure runtime metadata. When the user supplies
 * `procedures: { ... }` in `client({})`, each entry pins the kind +
 * idempotency flag for that procedure id. Without an entry, the proxy
 * falls back to the explicit `query()` / `mutation()` method names.
 */
export interface ProcedureMeta {
  kind: CallKind;
  idempotent?: boolean;
}

export interface ClientOptions<App = unknown> {
  /**
   * Origin to prefix every request with. Empty string for same-origin
   * (e.g. when the client runs in the same browser tab as the app).
   */
  baseUrl?: string;
  /**
   * Custom fetch impl. Useful for SSR (`node-fetch`), edge runtimes,
   * or test mocking. Defaults to globalThis.fetch.
   */
  fetch?: typeof globalThis.fetch;
  /**
   * Bearer token, or a (possibly async) resolver. Sent as
   * `Authorization: Bearer <token>` when the resolved value is truthy.
   */
  auth?: AuthValue;
  /**
   * Wire transformer. Must match `manifest.transformer` on the server
   * side or the wire will fail to decode. Defaults to "superjson".
   */
  transformer?: Transformer;
  /**
   * Opt into auto-batching for queries. Mutations and streams never
   * batch. Default false (additive opt-in).
   */
  batch?: boolean;
  /** Global error sink — fires for every rejected request. */
  onError?: (err: RpcError) => void;
  /** Fires once per UNAUTHENTICATED response (e.g. to redirect to login). */
  onAuthExpired?: () => void;
  /**
   * Explicit per-procedure metadata. Useful when the App type is
   * declared via the typeMarker fallback (no compile-time access to
   * procedure kinds). For queries that don't appear in this map, the
   * proxy still routes them by method name (`.query()` / `.mutation()`).
   */
  procedures?: Record<string, ProcedureMeta>;
  /**
   * Phantom — kept so `client<App>` is parametric. The type isn't read
   * at runtime; the generic threads through to the proxy's return type.
   */
  _phantom?: App;
}

// ── Per-call options for the proxy methods ─────────────────────────────

export interface CallOptions {
  signal?: AbortSignal;
  headers?: Record<string, string>;
  timeout?: number;
}

/** Internal call options used by the escape-hatch `call(id, ...)`. */
export interface FullCallOptions extends CallOptions {
  kind: CallKind;
  idempotent?: boolean;
}

// ── Type-level surface ─────────────────────────────────────────────────

/**
 * Marker type users attach to procedure ids when declaring an `App`
 * type. Each entry carries the kind + input/output types. Consumed by
 * `TypedClient<App>` below.
 *
 *   type App = {
 *     listTodos: { kind: "query"; input: { limit?: number }; output: Todo[] };
 *     addTodo:   { kind: "mutation"; input: { text: string }; output: Todo };
 *   };
 */
export interface ProcedureType<
  TKind extends CallKind = CallKind,
  TIn = unknown,
  TOut = unknown,
> {
  kind: TKind;
  input: TIn;
  output: TOut;
  idempotent?: boolean;
}

/**
 * Compute the per-procedure handle. `query` / `mutation` are always
 * present — calling the wrong one throws at runtime via dispatchKind.
 * `stream` returns an async-iter consuming the AI-SDK Data Stream
 * Protocol response. `streamUrl` gives the URL form for handing to
 * ai-sdk's `useChat`. `subscribe` opens a WebSocket subscription
 * and dispatches `{"t":"data"}` frames to `onData` once the
 * WebSocket transport is enabled.
 */
export interface ProcedureHandle<TIn = unknown, TOut = unknown> {
  query(input?: TIn, opts?: CallOptions): Promise<TOut>;
  mutation(input?: TIn, opts?: CallOptions): Promise<TOut>;
  stream(input?: TIn, opts?: CallOptions): AsyncIterableIterator<TOut>;
  /**
   * Returns the streaming URL for this procedure. Use when handing the
   * URL directly to ai-sdk's `useChat({ api: ... })` — the stream
   * itself is consumed by ai-sdk's parser, not this client.
   *
   * Returns a Promise<string> when the input requires async serialization
   * (the default with `transformer: "superjson"`); a plain string when
   * input is undefined (no body, no query-string).
   */
  streamUrl(input?: TIn): string | Promise<string>;
  /**
   * Open a WebSocket-backed subscription. Auto-reconnects on abnormal
   * closure (1006) with exponential backoff. Call `handle.unsubscribe()`
   * (or abort the supplied `signal`) to close cleanly.
   */
  subscribe(input?: TIn, opts?: SubscribeOptions<TOut>): SubscriptionHandle;
}

/**
 * Recursive type that converts `App` into the proxy surface. Dotted
 * ids ("todos.list") expand into nested objects (`rpc.todos.list`) so
 * both forms compile.
 */
export type TypedClient<App> = {
  [K in keyof App as ExtractTopSegment<K & string>]: NestedFor<App, K & string>;
} & UntypedClientMethods;

type ExtractTopSegment<K extends string> = K extends `${infer Head}.${string}`
  ? Head
  : K;

type NestedFor<App, FullKey extends string> = FullKey extends `${string}.${infer Rest}`
  ? // The top segment is `Head` (a key on `App`); the rest expands
    // recursively. We keep things shallow here and trust that direct
    // string-keyed access into the proxy is enough for users — the
    // proxy is the same object regardless of nesting depth.
    NestedShape<App, ExtractTopSegment<FullKey>>
  : ProcedureForKey<App, FullKey>;

type NestedShape<App, Top extends string> = {
  [K in keyof App as K extends `${Top}.${infer Rest}` ? Rest : never]: NestedForRest<
    App,
    K & string,
    Top
  >;
};

type NestedForRest<
  App,
  FullKey extends string,
  Top extends string,
> = FullKey extends `${Top}.${infer Rest}`
  ? Rest extends `${string}.${string}`
    ? NestedShape<App, ExtractTopSegment<Rest>>
    : ProcedureForKey<App, FullKey>
  : never;

type ProcedureForKey<App, K extends string> = K extends keyof App
  ? App[K] extends ProcedureType<infer _Kind, infer TIn, infer TOut>
    ? ProcedureHandle<TIn, TOut>
    : ProcedureHandle
  : ProcedureHandle;

interface UntypedClientMethods {
  /**
   * Escape-hatch dispatch. Useful when calling procedures whose ids
   * aren't part of the `App` type (e.g. dynamically-named handlers).
   *
   * For `kind: "query"` / `"mutation"` returns `Promise<TOut>`.
   * For `kind: "stream"` returns `AsyncIterableIterator<TOut>`.
   */
  call<TOut = unknown>(
    procId: string,
    input?: unknown,
    opts?: FullCallOptions,
  ): Promise<TOut> | AsyncIterableIterator<TOut>;
}

// ── Runtime kind inference (for type-only mode) ────────────────────────

const QUERY_PREFIXES = /^(get|list|find|search|count|read|fetch)([A-Z_]|$)/;

function inferKindFromName(id: string): CallKind {
  // Use the leaf segment for inference (e.g. "todos.list" → "list").
  const leaf = id.includes(".") ? id.slice(id.lastIndexOf(".") + 1) : id;
  if (QUERY_PREFIXES.test(leaf)) return "query";
  return "mutation";
}

// ── Builder ────────────────────────────────────────────────────────────

/**
 * Construct a typed client.
 *
 * Type parameter `App` is optional; without it, the proxy is still
 * usable via `rpc.<id>.query()` / `rpc.<id>.mutation()` — but the
 * input/output sides are `unknown`. Pair with `procedures: {...}` for
 * runtime kind dispatch.
 */
export function client<App = Record<string, never>>(
  options: ClientOptions<App> = {},
): TypedClient<App> {
  const baseUrl = options.baseUrl ?? "";
  const fetchFn = options.fetch ?? globalThis.fetch?.bind(globalThis);
  if (!fetchFn) {
    throw new Error(
      "[zeroship/rpc-client] no fetch implementation — pass `fetch:` in client({}) or run on a runtime that exposes globalThis.fetch.",
    );
  }
  const transformer: Transformer = options.transformer ?? "superjson";
  const proceduresMeta = options.procedures ?? {};

  // Resolve auth lazily — same input shape supported as `AuthValue`.
  const authResolver: () => string | null | undefined | Promise<string | null | undefined> =
    typeof options.auth === "function"
      ? options.auth
      : options.auth !== undefined
        ? () => options.auth as string
        : () => null;

  const transportCfg: TransportConfig = {
    baseUrl,
    fetch: fetchFn,
    transformer,
    authResolver,
    onError: options.onError,
    onAuthExpired: options.onAuthExpired,
  };

  const batchLink: BatchLink | null = options.batch
    ? createBatchLink({
        baseUrl,
        fetch: fetchFn,
        transformer,
        authResolver,
        onError: options.onError,
        onAuthExpired: options.onAuthExpired,
      })
    : null;

  /** Send one unary call, optionally routing through the batch link. */
  function dispatch<T>(
    procId: string,
    input: unknown,
    fullOpts: FullCallOptions,
  ): Promise<T> {
    // Route queries through the batch link when:
    //   - batch is enabled
    //   - the call has no AbortSignal (batched calls share fate)
    //   - no per-call headers (would need per-call header support in batch)
    if (
      batchLink &&
      fullOpts.kind === "query" &&
      !fullOpts.signal &&
      !fullOpts.headers
    ) {
      return batchLink.enqueue<T>(procId, input);
    }
    return sendUnary<T>(procId, input, transportCfg, fullOpts);
  }

  // ── Proxy assembly ────────────────────────────────────────────────────

  function makeProcedureHandle(procId: string): ProcedureHandle {
    const meta = proceduresMeta[procId];
    return {
      query(input?: unknown, opts?: CallOptions) {
        return dispatch(procId, input, {
          ...opts,
          kind: "query",
          idempotent: meta?.idempotent ?? false,
        });
      },
      mutation(input?: unknown, opts?: CallOptions) {
        return dispatch(procId, input, {
          ...opts,
          kind: "mutation",
          idempotent: meta?.idempotent ?? false,
        });
      },
      stream(input?: unknown, opts?: CallOptions): AsyncIterableIterator<unknown> {
        return streamCall(procId, input, transportCfg, opts ?? {});
      },
      streamUrl(input?: unknown): string | Promise<string> {
        return buildStreamUrl(procId, input, transportCfg);
      },
      subscribe(input?: unknown, opts?: SubscribeOptions<unknown>): SubscriptionHandle {
        return subscribeCall(procId, input, transportCfg, opts ?? {});
      },
    };
  }

  /**
   * Recursive proxy node. `currentPath` is the dotted prefix
   * accumulated so far (e.g. ["todos", "list"]). Reading a string
   * property either:
   *   - returns a deeper Proxy (path not yet a known leaf), or
   *   - returns a ProcedureHandle (path matches a leaf id).
   *
   * Because we can't know in advance whether "todos.list" or "todos"
   * is the leaf, every node is BOTH:
   *   - a ProcedureHandle for procId = currentPath.join(".")
   *   - a parent for sub-paths
   *
   * Property reads on the handle (`.query`, `.mutation`, etc.) hit the
   * underlying handle methods; reads of any other string key descend
   * into a deeper proxy.
   */
  function makeNode(currentPath: string[]): unknown {
    const procId = currentPath.join(".");
    const handle = currentPath.length > 0 ? makeProcedureHandle(procId) : null;

    const target = function () {} as unknown as object;
    return new Proxy(target, {
      get(_target, prop): unknown {
        if (typeof prop !== "string") return undefined;
        // Top-level escape hatches & internals only resolve at the root.
        if (currentPath.length === 0) {
          if (prop === "call") {
            return function callProc<TOut = unknown>(
              id: string,
              input?: unknown,
              opts?: FullCallOptions,
            ): Promise<TOut> | AsyncIterableIterator<TOut> {
              const kind = opts?.kind ?? inferKindFromName(id);
              if (kind === "stream") {
                return streamCall<TOut>(id, input, transportCfg, {
                  signal: opts?.signal,
                  headers: opts?.headers,
                  timeout: opts?.timeout,
                });
              }
              return dispatch<TOut>(id, input, {
                ...opts,
                kind,
                idempotent: opts?.idempotent ?? false,
              });
            };
          }
          if (prop === "then" || prop === "catch" || prop === "finally") {
            // The root is NOT a thenable. Returning undefined here keeps
            // accidental `await rpc` from hanging on the proxy itself.
            return undefined;
          }
        }
        // ProcedureHandle methods at any non-root depth.
        if (handle) {
          if (prop === "query") return handle.query.bind(handle);
          if (prop === "mutation") return handle.mutation.bind(handle);
          if (prop === "stream") return handle.stream.bind(handle);
          if (prop === "streamUrl") return handle.streamUrl.bind(handle);
          if (prop === "subscribe") return handle.subscribe.bind(handle);
          // Avoid `then`/`Symbol.iterator`-driven misuses bouncing into
          // a deeper proxy.
          if (prop === "then" || prop === "catch" || prop === "finally") {
            return undefined;
          }
        }
        // Descend.
        return makeNode([...currentPath, prop]);
      },
    });
  }

  return makeNode([]) as TypedClient<App>;
}
