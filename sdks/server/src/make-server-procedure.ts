//
// `__makeServerProcedure(impl, meta)` — server-side counterpart to
// `__makeProcedure` from `@zeroship/rpc-client`. Used by the
// vite-plugin's SSR-enabled-app variant: the same component code that
// calls `list.useQuery(...)` works on both server and client because
// both wrappers expose an identical hook surface.
//
// Server semantics:
//
//   - `proc(input)` — direct call to `impl(input)`. No HTTP. The
//     synthetic SSR entry uses this path; the kernel's fast path
//     dispatches to it directly.
//   - `proc.useQuery(input, options?)` — wraps React Query's
//     `useQuery` with a `queryFn` that calls `impl(input)`. Per-
//     request `<QueryClientProvider client={qc}>` lands the result
//     in `qc`, which the worker then `dehydrate(qc)`s to JSON for
//     the client to hydrate.
//   - `proc.prefetch(input, qc)` — populates the QueryClient cache
//     before render. Useful for route-level data dependencies that
//     should resolve before any component sees them.
//   - `proc.useMutation` / `useStream` / `useSubscription` — throw
//     INTERNAL during SSR. Mutations are user-action triggered;
//     subscriptions need a long-lived connection. These don't make
//     sense to render server-side.
//
// React Query is an OPTIONAL peer dep. When the user authors an
// SSR-enabled app, the vite-plugin guarantees React Query is in
// `devDependencies`. The dynamic import below makes the module load
// gracefully even when consumer apps haven't installed it (e.g. a
// pure RPC-only app that re-exports its server module without SSR
// wrapping).

import type { ProcedureKind } from "./types.js";

// React Query is loaded lazily — keep core `@zeroship/server` import
// graph free of `@tanstack/react-query`.
//
// The cast through `unknown` keeps TypeScript happy when the peer dep
// isn't installed in the type-check sandbox: types resolve when the
// user actually installs it; absent the install, `unknown` falls back
// to the runtime check below.
type ReactQueryModule = {
  useQuery: (config: { queryKey: unknown[]; queryFn: () => Promise<unknown> }) => {
    data: unknown;
    isSuccess: boolean;
    isLoading: boolean;
    isError: boolean;
    error: unknown;
  };
  useSuspenseQuery: (config: {
    queryKey: unknown[];
    queryFn: () => Promise<unknown>;
  }) => { data: unknown };
};

interface QueryClientLike {
  prefetchQuery: (args: {
    queryKey: unknown[];
    queryFn: () => Promise<unknown>;
  }) => Promise<unknown>;
}

let _reactQuery: ReactQueryModule | null = null;
let _reactQueryLoadAttempted = false;
async function loadReactQuery(): Promise<ReactQueryModule | null> {
  if (_reactQuery) return _reactQuery;
  if (_reactQueryLoadAttempted) return null;
  _reactQueryLoadAttempted = true;
  try {
    // @ts-ignore — optional peer dep; ts can't always resolve without install.
    const m = await import("@tanstack/react-query");
    _reactQuery = m as unknown as ReactQueryModule;
    return _reactQuery;
  } catch {
    return null;
  }
}

// Synchronous variant — uses the cached module if present, returns
// null otherwise. Used by the React-Hooks call sites (which can't
// await; they need a synchronous reference).
function reactQuerySync(): ReactQueryModule | null {
  return _reactQuery;
}

// Eagerly attempt to load React Query at module evaluation. The
// promise is awaited inside `useQuery` getters' first invocation;
// subsequent calls hit the cache. SSR runs single-threaded inside the
// worker, so the await is fine — it adds one microtask to the first
// render but never blocks.
//
// We don't `await` here because the module has consumers that don't
// need React Query (RPC-only apps). The cached promise resolves
// before the first SSR `useQuery` call in practice because importing
// `@zeroship/server` is generally synchronous wrt the React render.
void loadReactQuery();

/** Per-procedure metadata mirrored from the client adapter. */
export interface ServerProcedureMeta {
  /** Wire id — same as the client uses for queryKey routing. */
  id: string;
  /** Discriminator. */
  kind: ProcedureKind;
}

/**
 * Wrap a server procedure implementation in the hooks-on-function
 * surface required by SSR. Returns a callable that ALSO carries
 * `useQuery`, `useSuspenseQuery`, `prefetch`, etc. when the procedure
 * is a query.
 *
 * Mutations / streams / subscriptions are no-ops at SSR time — the
 * corresponding hooks throw INTERNAL with a clear message.
 *
 * @param impl  The actual handler, called directly (no HTTP).
 * @param meta  `{ id, kind }` — same shape as the client adapter.
 */
export function __makeServerProcedure<TIn = unknown, TOut = unknown>(
  impl: (input: TIn) => Promise<TOut> | AsyncIterable<TOut>,
  meta: ServerProcedureMeta,
): ServerProcedureFn<TIn, TOut> {
  // Plain callable — direct dispatch to impl.
  const fn = function (input: TIn): Promise<TOut> | AsyncIterable<TOut> {
    return impl(input);
  } as unknown as ServerProcedureFn<TIn, TOut>;

  Object.defineProperty(fn, "id", { value: meta.id, enumerable: true });
  Object.defineProperty(fn, "kind", { value: meta.kind, enumerable: true });
  Object.defineProperty(fn, "queryKey", {
    value: (input?: TIn): [string, ...unknown[]] =>
      input === undefined ? [meta.id] : [meta.id, input],
    enumerable: true,
  });

  if (meta.kind === "query") {
    Object.defineProperty(fn, "useQuery", {
      value: (input: TIn, options?: Record<string, unknown>) => {
        const rq = reactQuerySync();
        if (!rq) {
          throw new Error(
            "[zeroship/server] @tanstack/react-query is required for server-side useQuery — install it as a peer dependency.",
          );
        }
        return rq.useQuery({
          queryKey: input === undefined ? [meta.id] : [meta.id, input],
          queryFn: () => impl(input) as Promise<unknown>,
          ...options,
        });
      },
    });
    Object.defineProperty(fn, "useSuspenseQuery", {
      value: (input: TIn, options?: Record<string, unknown>) => {
        const rq = reactQuerySync();
        if (!rq) {
          throw new Error(
            "[zeroship/server] @tanstack/react-query is required for server-side useSuspenseQuery — install it as a peer dependency.",
          );
        }
        return rq.useSuspenseQuery({
          queryKey: input === undefined ? [meta.id] : [meta.id, input],
          queryFn: () => impl(input) as Promise<unknown>,
          ...options,
        });
      },
    });
    Object.defineProperty(fn, "prefetch", {
      value: async (input: TIn, qc: QueryClientLike): Promise<unknown> => {
        return qc.prefetchQuery({
          queryKey: input === undefined ? [meta.id] : [meta.id, input],
          queryFn: () => impl(input) as Promise<unknown>,
        });
      },
    });
  }

  if (meta.kind === "mutation") {
    Object.defineProperty(fn, "useMutation", {
      value: (_options?: Record<string, unknown>): never => {
        throw new Error(
          `[zeroship/server] useMutation is a no-op during SSR — mutations run on user action, not during render. (proc: ${meta.id})`,
        );
      },
    });
  }

  if (meta.kind === "stream") {
    Object.defineProperty(fn, "useStream", {
      value: (_input: TIn, _options?: Record<string, unknown>): never => {
        throw new Error(
          `[zeroship/server] useStream is a no-op during SSR — streams need a long-lived connection that doesn't fit a render pass. (proc: ${meta.id})`,
        );
      },
    });
  }

  if (meta.kind === "subscription") {
    Object.defineProperty(fn, "useSubscription", {
      value: (_input: TIn, _options?: Record<string, unknown>): never => {
        throw new Error(
          `[zeroship/server] useSubscription is a no-op during SSR — subscriptions need a long-lived connection. (proc: ${meta.id})`,
        );
      },
    });
  }

  if (meta.kind === "action") {
    // B3 — actions are imperative (fetch + runMutation). The hook
    // mirrors mutation: it's a no-op during SSR because actions run
    // on user interaction, not during render.
    Object.defineProperty(fn, "useAction", {
      value: (_options?: Record<string, unknown>): never => {
        throw new Error(
          `[zeroship/server] useAction is a no-op during SSR — actions run on user interaction, not during render. (proc: ${meta.id})`,
        );
      },
    });
  }

  return fn;
}

// ── Procedure types ────────────────────────────────────────────────

export interface ServerProcedureCommon<TIn, TOut> {
  (input: TIn): Promise<TOut> | AsyncIterable<TOut>;
  id: string;
  kind: ProcedureKind;
  queryKey: (input?: TIn) => [string, ...unknown[]];
}

export interface ServerQueryProcedure<TIn, TOut>
  extends ServerProcedureCommon<TIn, TOut> {
  useQuery: (input: TIn, options?: Record<string, unknown>) => unknown;
  useSuspenseQuery: (input: TIn, options?: Record<string, unknown>) => unknown;
  prefetch: (input: TIn, qc: QueryClientLike) => Promise<unknown>;
}

export interface ServerMutationProcedure<TIn, TOut>
  extends ServerProcedureCommon<TIn, TOut> {
  useMutation: (options?: Record<string, unknown>) => never;
}

export interface ServerStreamProcedure<TIn, TOut>
  extends ServerProcedureCommon<TIn, TOut> {
  useStream: (input: TIn, options?: Record<string, unknown>) => never;
}

export interface ServerSubscriptionProcedure<TIn, TOut>
  extends ServerProcedureCommon<TIn, TOut> {
  useSubscription: (input: TIn, options?: Record<string, unknown>) => never;
}

/**
 * B3 — `action` capability. Imperative wrapper that may call `fetch`,
 * `runQuery`, `runMutation`. Hook is a no-op during SSR (actions run
 * on user interaction).
 */
export interface ServerActionProcedure<TIn, TOut>
  extends ServerProcedureCommon<TIn, TOut> {
  useAction: (options?: Record<string, unknown>) => never;
}

export type ServerProcedureFn<TIn, TOut> =
  | ServerQueryProcedure<TIn, TOut>
  | ServerMutationProcedure<TIn, TOut>
  | ServerStreamProcedure<TIn, TOut>
  | ServerSubscriptionProcedure<TIn, TOut>
  | ServerActionProcedure<TIn, TOut>;
