// packages/zeroship-rpc-client/src/make-procedure.ts
//
// `__makeProcedure(call, meta)` — wraps a raw HTTP-RPC call in a
// callable + hooks-on-function object. Per spec §10:
//
//   const list = __makeProcedure(input => callList(input), {
//     id: "todos.list", kind: "query"
//   });
//
//   await list({ limit: 50 });           // direct call
//   list.useQuery({ limit: 50 });        // React hook (when @zeroship/rpc-react is loaded)
//   list.invalidate();                   // queryClient.invalidateQueries(["todos.list"])
//   list.prefetch({ limit: 50 });        // queryClient.prefetchQuery(...)
//
// Hooks are attached via `Object.defineProperty` getters that read
// from `_hookRegistry`. The registry is populated as a side effect of
// importing `@zeroship/rpc-react`. Procedures in non-React bundles
// (Vue, Solid, vanilla) carry only the call surface — React Query
// never enters the bundle until something imports `@zeroship/rpc-react`.
//
// The vite-plugin transform's client output (RPC-only client variant)
// emits one `__makeProcedure(...)` per server export. The hooks
// surface attaches uniformly regardless of whether the caller ends up
// using React.

import { _hookRegistry, HOOK_UNAVAILABLE_MESSAGE } from "./_hooks.js";
import { newUuidV7 } from "./idempotency.js";

/** Procedure kind — matches the wire protocol's discriminator. */
export type ProcedureKind = "query" | "mutation" | "stream" | "subscription";

/**
 * Per-procedure metadata. The build-time transform injects this; users
 * supplying procedures by hand pass it explicitly.
 */
export interface ProcedureBuildMeta {
  /** Wire id — stable across refactors, used as the queryKey prefix. */
  id: string;
  /** Discriminator: drives which hooks attach. */
  kind: ProcedureKind;
  /**
   * Mutations only: when true, the React adapter generates a fresh
   * Idempotency-Key per `mutate()` call and reuses it across retries
   * (the React Query observer's lifetime). The server's Phase 6
   * dedupe table replays the first response on retry.
   */
  idempotent?: boolean;
}

/**
 * Caller signature. Always async; returns the procedure's output.
 * Stream procedures' `call` returns the AsyncIterable directly (the
 * caller wraps it in `useStream`); for queries / mutations the return
 * is the unwrapped value.
 */
export type ProcedureCaller<TIn, TOut> = (
  input: TIn,
  options?: { idempotencyKey?: string },
) => Promise<TOut> | AsyncIterable<TOut>;

// ── Hook-adapter shapes (loose) ─────────────────────────────────────
//
// We keep these typed as `unknown`-returning functions taking a single
// config object. The concrete return types are React Query's
// `UseQueryResult`, `UseMutationResult`, etc., but typing those here
// would force a hard `@tanstack/react-query` dep on `@zeroship/rpc-client`.
// The React adapter (`@zeroship/rpc-react`) re-exports `useQuery`
// directly when the precise types are needed at the user's source.

type AdapterHook = (config: Record<string, unknown>) => unknown;

interface QueryClientLike {
  invalidateQueries: (args: { queryKey: unknown[] }) => Promise<void> | void;
  prefetchQuery: (args: {
    queryKey: unknown[];
    queryFn: () => Promise<unknown>;
  }) => Promise<unknown>;
  setQueryData: (key: unknown[], updater: unknown) => unknown;
}

// ── Procedure handle types ──────────────────────────────────────────
//
// Each kind exposes a different subset of helpers. The intersection
// type below (`ProcedureFn`) is a discriminated union narrowed by
// `kind` at runtime; users typically interact with the result via
// the kind they declared (e.g. `add.useMutation` is only valid on a
// mutation).

interface ProcedureCommon<TIn, TOut> {
  (input: TIn, options?: { idempotencyKey?: string }):
    | Promise<TOut>
    | AsyncIterable<TOut>;
  id: string;
  kind: ProcedureKind;
  queryKey: (input?: TIn) => [string, ...unknown[]];
}

export interface QueryProcedure<TIn, TOut> extends ProcedureCommon<TIn, TOut> {
  useQuery: (input: TIn, options?: Record<string, unknown>) => unknown;
  useSuspenseQuery: (input: TIn, options?: Record<string, unknown>) => unknown;
  useInfiniteQuery: (
    input: TIn,
    options?: Record<string, unknown>,
  ) => unknown;
  invalidate: (input?: TIn) => Promise<void> | void;
  prefetch: (input: TIn, options?: Record<string, unknown>) => Promise<unknown>;
  setData: (input: TIn, updater: unknown) => unknown;
}

export interface MutationProcedure<TIn, TOut>
  extends ProcedureCommon<TIn, TOut> {
  useMutation: (options?: Record<string, unknown>) => unknown;
}

export interface StreamProcedure<TIn, TOut>
  extends ProcedureCommon<TIn, TOut> {
  useStream: (input: TIn, options?: Record<string, unknown>) => unknown;
}

export interface SubscriptionProcedure<TIn, TOut>
  extends ProcedureCommon<TIn, TOut> {
  useSubscription: (input: TIn, options?: Record<string, unknown>) => unknown;
}

export type ProcedureFn<TIn, TOut> =
  | QueryProcedure<TIn, TOut>
  | MutationProcedure<TIn, TOut>
  | StreamProcedure<TIn, TOut>
  | SubscriptionProcedure<TIn, TOut>;

// ── Implementation ──────────────────────────────────────────────────

/** Lazily resolve a hook from the registry, throwing a clear install hint when missing. */
function requireHook(name: keyof typeof _hookRegistry): AdapterHook {
  const hook = _hookRegistry[name];
  if (typeof hook !== "function") {
    throw new Error(HOOK_UNAVAILABLE_MESSAGE);
  }
  // We don't gate on `providerMounted` here — React Query itself throws
  // a clear "no QueryClient set" error when its own context is missing,
  // and tests legitimately call hooks without rendering through the
  // <ZeroshipProvider>. The `providerMounted` flag is exposed for
  // diagnostic purposes only.
  return hook as AdapterHook;
}

function requireQueryClient(): QueryClientLike {
  const qc = _hookRegistry.queryClient;
  if (!qc) {
    throw new Error(HOOK_UNAVAILABLE_MESSAGE);
  }
  return qc as QueryClientLike;
}

/**
 * Wrap a raw HTTP-RPC call into the procedure handle described in §10.
 *
 * `call` is the closure that performs the underlying HTTP request and
 * returns the procedure output. For mutations, it accepts an optional
 * `idempotencyKey` so the React adapter can reuse the same key across
 * retries.
 *
 * The returned object IS callable: `proc(input)` invokes `call(input)`.
 * Hooks attach via `Object.defineProperty` getters that read from
 * `_hookRegistry` — undefined slots throw the install hint.
 */
export function __makeProcedure<TIn = unknown, TOut = unknown>(
  call: ProcedureCaller<TIn, TOut>,
  meta: ProcedureBuildMeta,
): ProcedureFn<TIn, TOut> {
  // Callable scaffolding. `target` is the raw function; we attach
  // properties directly to it so `proc(input)` and `proc.useQuery(input)`
  // both work without an intermediate proxy.
  const fn = function (
    input: TIn,
    options?: { idempotencyKey?: string },
  ): Promise<TOut> | AsyncIterable<TOut> {
    return call(input, options);
  } as unknown as ProcedureFn<TIn, TOut>;

  // ── Common bag ──
  Object.defineProperty(fn, "id", { value: meta.id, enumerable: true });
  Object.defineProperty(fn, "kind", { value: meta.kind, enumerable: true });
  Object.defineProperty(fn, "queryKey", {
    value: (input?: TIn): [string, ...unknown[]] =>
      input === undefined ? [meta.id] : [meta.id, input],
    enumerable: true,
  });

  // ── Per-kind hooks ──
  if (meta.kind === "query") {
    Object.defineProperty(fn, "useQuery", {
      enumerable: true,
      configurable: true,
      get() {
        const useQuery = requireHook("useQuery");
        return (input: TIn, options?: Record<string, unknown>) =>
          useQuery({
            queryKey: input === undefined ? [meta.id] : [meta.id, input],
            queryFn: () => call(input),
            ...options,
          });
      },
    });
    Object.defineProperty(fn, "useSuspenseQuery", {
      enumerable: true,
      configurable: true,
      get() {
        const useSuspenseQuery = requireHook("useSuspenseQuery");
        return (input: TIn, options?: Record<string, unknown>) =>
          useSuspenseQuery({
            queryKey: input === undefined ? [meta.id] : [meta.id, input],
            queryFn: () => call(input),
            ...options,
          });
      },
    });
    Object.defineProperty(fn, "useInfiniteQuery", {
      enumerable: true,
      configurable: true,
      get() {
        const useInfiniteQuery = requireHook("useInfiniteQuery");
        return (input: TIn, options?: Record<string, unknown>) =>
          useInfiniteQuery({
            queryKey: input === undefined ? [meta.id] : [meta.id, input],
            queryFn: ({ pageParam }: { pageParam?: unknown }) =>
              call(
                input === undefined
                  ? ({ cursor: pageParam } as unknown as TIn)
                  : ({ ...input, cursor: pageParam } as unknown as TIn),
              ),
            getNextPageParam: (last: unknown) =>
              (last as { nextCursor?: unknown } | null)?.nextCursor ??
              undefined,
            initialPageParam: undefined,
            ...options,
          });
      },
    });
    Object.defineProperty(fn, "invalidate", {
      enumerable: true,
      configurable: true,
      value: (input?: TIn) => {
        const qc = requireQueryClient();
        return qc.invalidateQueries({
          queryKey: input === undefined ? [meta.id] : [meta.id, input],
        });
      },
    });
    Object.defineProperty(fn, "prefetch", {
      enumerable: true,
      configurable: true,
      value: (input: TIn, options?: Record<string, unknown>) => {
        const qc = requireQueryClient();
        return qc.prefetchQuery({
          queryKey: input === undefined ? [meta.id] : [meta.id, input],
          queryFn: () => call(input) as Promise<unknown>,
          ...options,
        });
      },
    });
    Object.defineProperty(fn, "setData", {
      enumerable: true,
      configurable: true,
      value: (input: TIn, updater: unknown) => {
        const qc = requireQueryClient();
        return qc.setQueryData(
          input === undefined ? [meta.id] : [meta.id, input],
          updater,
        );
      },
    });
  }

  // ── Per-procedure idempotency-key tracking ────────────────────────
  //
  // Lives at procedure scope (closes over `idempotencyKeysByInput`
  // below), NOT inside the `useMutation` getter. That way React
  // re-renders during a retry cycle don't reset the map — every
  // retry of a single `mutate()` looks up the key by the variables'
  // identity and finds the same entry.
  //
  // WeakMap auto-collects when the input goes out of scope. The
  // typical mutate-once-pass-an-object pattern (`mutate({ text })`)
  // gets the GC for free.
  const idempotencyKeysByInput = new WeakMap<object, string>();

  if (meta.kind === "mutation") {
    Object.defineProperty(fn, "useMutation", {
      enumerable: true,
      configurable: true,
      get() {
        const useMutation = requireHook("useMutation");
        return (options?: Record<string, unknown>) => {
          // Per spec §10 "Idempotency × retry interaction": when
          // `meta.idempotent === true`, we generate ONE key per
          // `mutate()` call and reuse it across React Query retries.
          //
          // The model:
          //
          //   - On a NEW `mutate(input)` call, allocate a fresh key
          //     and stash it in `idempotencyKeysByInput[input]`.
          //   - On retry, React Query passes the SAME `input`
          //     reference to the (possibly re-rendered) mutationFn;
          //     we look up the existing entry and reuse it.
          //   - When the mutation cycle settles (success OR final
          //     error), we delete the entry so a fresh `mutate(input)`
          //     with the same input reference gets a fresh key.
          //
          // Crucially, `idempotencyKeysByInput` lives at PROCEDURE
          // scope, not inside this getter. React re-renders re-invoke
          // the getter and produce a new mutationFn closure, but the
          // map persists — so retry-dispatch finds the existing key.
          //
          // We hook the lifecycle by wrapping `onSettled` rather than
          // by tracking attempts inside the mutationFn (which can't
          // know whether it's attempt 1 or 4). On settle we clean up.
          const userOpts = (options ?? {}) as Record<string, unknown>;
          const userOnSettled = userOpts.onSettled as
            | ((
                data: unknown,
                err: unknown,
                input: unknown,
                ctx: unknown,
              ) => unknown)
            | undefined;

          const wrappedMutationFn = (input: TIn) => {
            let key: string | undefined;
            if (meta.idempotent) {
              if (input !== null && typeof input === "object") {
                const obj = input as object;
                const existing = idempotencyKeysByInput.get(obj);
                if (existing) {
                  key = existing;
                } else {
                  key = newUuidV7();
                  idempotencyKeysByInput.set(obj, key);
                }
              } else {
                // Non-object inputs can't live in a WeakMap. Fall
                // back to a fresh key per attempt — no retry dedupe,
                // but the common case (`{ text: "..." }`) hits the
                // tracked branch.
                key = newUuidV7();
              }
            }
            return call(input, key ? { idempotencyKey: key } : undefined);
          };

          const wrappedOnSettled = (
            data: unknown,
            err: unknown,
            input: unknown,
            ctx: unknown,
          ): unknown => {
            // Clear the per-input slot so a fresh `mutate(sameInput)`
            // gets a fresh key. WeakMap.delete is a no-op on missing.
            if (
              meta.idempotent &&
              input !== null &&
              typeof input === "object"
            ) {
              idempotencyKeysByInput.delete(input as object);
            }
            return userOnSettled?.(data, err, input, ctx);
          };

          return useMutation({
            mutationKey: [meta.id],
            ...userOpts,
            // Place these AFTER ...userOpts so we always win the
            // merge. Users who want a custom mutationFn should
            // factor it differently — direct `__makeProcedure`
            // usage already gives them the call closure.
            mutationFn: wrappedMutationFn,
            onSettled: wrappedOnSettled,
          });
        };
      },
    });
  }

  if (meta.kind === "stream") {
    Object.defineProperty(fn, "useStream", {
      enumerable: true,
      configurable: true,
      get() {
        const useStream = requireHook("useStream");
        return (input: TIn, options?: Record<string, unknown>) =>
          useStream({
            key: input === undefined ? [meta.id] : [meta.id, input],
            stream: () => call(input) as AsyncIterable<TOut>,
            ...options,
          });
      },
    });
  }

  if (meta.kind === "subscription") {
    Object.defineProperty(fn, "useSubscription", {
      enumerable: true,
      configurable: true,
      get() {
        const useSubscription = requireHook("useSubscription");
        return (input: TIn, options?: Record<string, unknown>) =>
          useSubscription({
            key: input === undefined ? [meta.id] : [meta.id, input],
            subscribe: (cb: unknown) => call(input, { idempotencyKey: undefined } as never),
            ...options,
          });
      },
    });
  }

  return fn;
}
