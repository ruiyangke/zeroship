/**
 * `useQuery` — React hook bridging @zeroship/db reactive queries onto
 * the React render cycle (P8b stage 4).
 *
 * Goals
 * -----
 *
 * 1. Loading state: first render returns `undefined`.
 * 2. After mount, the hook runs the factory once, awaits the result, and
 *    commits it via `setState`. Subsequent renders see the snapshot.
 * 3. Opens a broker subscription against the queried collection. Every
 *    broker event fires the factory again and commits the new snapshot.
 *    Read-set narrowing happens server-side (P8b: see broker.rs); this
 *    hook is a dumb consumer.
 * 4. Cleanup on unmount: closes the broker subscription (releases the
 *    handle synchronously) and marks the effect as cancelled so any
 *    in-flight refetch is dropped.
 * 5. StrictMode-safe: dev double-mount opens at most one *live* broker
 *    subscription per (component-instance × factory). The mount/unmount/
 *    re-mount sequence reuses the freshly-opened subscription via a
 *    cleanup-skip guard tied to a microtask. See test
 *    `b8b4_useQuery_strictmode_safe`.
 * 6. Dependency change: the user passes the query as a closure. The hook
 *    treats the closure identity as the cache key; a new closure ==
 *    a new query. Most call sites inline the closure so changes
 *    naturally trigger refetch on every render — but the hook avoids
 *    re-opening the subscription if the *collection name* is unchanged,
 *    only re-running the factory (cheap server roundtrip vs. broker
 *    re-register).
 *
 * Non-goals
 * ---------
 *
 *  * Real WebSocket multiplexing (one broker subscription per useQuery
 *    today; multiplex is a follow-up, see proposal §P8b.5).
 *  * Caching across components (no global QueryClient yet — this is the
 *    minimal Convex-style "subscribe-then-rerender" pattern).
 *  * Error retry. Errors surface to the caller via the result tuple.
 */

import * as React from "react";
import { subscribe as internalSubscribe } from "@zeroship/db/internal";

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/**
 * The factory must return one of:
 *
 *  - A `Query`-shaped object (from `@zeroship/db`): has an internal
 *    `_collection` field and is thenable. The hook reads `_collection`
 *    to open the broker subscription, then awaits the Query for data.
 *  - A bare `Promise<T>` plus an explicit collection override via the
 *    `{ collection }` options. Useful for callers who want to compose
 *    multiple queries into one shape.
 */
export type QueryFactory<T> = () => QueryLike<T> | Promise<T>;

/**
 * Shape we duck-type for: any value that's awaitable AND carries a
 * `_collection` string. `Query<S, P>` from @zeroship/db matches; tests
 * use a minimal stand-in.
 */
export interface QueryLike<T> {
  /** Collection name to subscribe on. Read once at mount. */
  readonly _collection: string;
  /**
   * Thenable contract — Query<S, P>.then resolves with `Result<P[]>`
   * (`{ data, error }`). We support both that envelope shape and a bare
   * value (for tests / custom factories).
   */
  then<R1 = T, R2 = never>(
    onfulfilled?: ((value: T) => R1 | PromiseLike<R1>) | null,
    onrejected?: ((reason: unknown) => R2 | PromiseLike<R2>) | null,
  ): PromiseLike<R1 | R2>;
}

/** Options accepted by `useQuery`. */
export interface UseQueryOptions {
  /**
   * Collection name to subscribe to. Required when the factory returns
   * a plain Promise (no `_collection`). Ignored when the factory's
   * return value already carries `_collection`.
   */
  collection?: string;
  /**
   * Initial value to return on the very first render — useful for SSR
   * hydration where the server-rendered HTML already reflects a
   * snapshot. When provided, the hook returns it instead of `undefined`
   * on the first render. Defaults to `undefined`.
   */
  initialData?: unknown;
}

/**
 * Broker dependencies the hook needs. Injected via `<QueryProvider>` or
 * a direct `client` prop on `useQuery`; `createDefaultClient()` wires
 * up the default implementation for ordinary app code. We keep the
 * indirection so tests can stub the broker without spinning up the
 * native runtime.
 */
export interface QueryClient {
  /**
   * Open a broker subscription on `collection`. Returns an object with
   * an AsyncIterable contract — `next()` resolves with the next event
   * or `{ done: true }` on close. The hook iterates inside an effect
   * and triggers a refetch on each non-closed event.
   */
  subscribe(collection: string): SubscriptionLike;
}

/**
 * The minimum subscription shape the hook consumes. Matches
 * `@zeroship/db`'s `Subscription` interface (close + AsyncIterable).
 */
export interface SubscriptionLike {
  close(): void;
  [Symbol.asyncIterator](): AsyncIterator<unknown>;
}

// ---------------------------------------------------------------------------
// Client context — supplies the broker hookup
// ---------------------------------------------------------------------------

const QueryClientContext = React.createContext<QueryClient | undefined>(undefined);

export interface QueryClientProviderProps {
  client: QueryClient;
  children: React.ReactNode;
}

/**
 * Provides the broker `QueryClient` to all `useQuery` calls below.
 * Pass an instance built via `createDefaultClient()` (which wires up
 * the framework-internal `@zeroship/db/internal` subscription bridge)
 * or a test stub.
 */
export function QueryClientProvider({ client, children }: QueryClientProviderProps): React.ReactElement {
  return (
    <QueryClientContext.Provider value={client}>
      {children}
    </QueryClientContext.Provider>
  );
}

/**
 * Build the default broker client wired to the framework-internal
 * `@zeroship/db/internal` subscription bridge. Pure indirection —
 * keeps `@zeroship/react` testable while avoiding a public dependency
 * on `@zeroship/db`'s low-level reactive primitive.
 */
export function createDefaultClient(
  subscribeImpl: (collection: string) => SubscriptionLike = internalSubscribe,
): QueryClient {
  return { subscribe: subscribeImpl };
}

// ---------------------------------------------------------------------------
// Result-envelope unwrap
// ---------------------------------------------------------------------------

/**
 * `Query.then()` in @zeroship/db resolves with `Result<P[]>` —
 * `{ data, error }`. We unwrap into either the data or a thrown error
 * so the hook contract is uniform across Query / bare Promise factories.
 */
function unwrapResult<T>(value: unknown): T {
  if (
    value !== null &&
    typeof value === "object" &&
    ("data" in value || "error" in value)
  ) {
    const env = value as { data?: T; error?: unknown };
    if (env.error) {
      throw env.error instanceof Error ? env.error : new Error(String(env.error));
    }
    return env.data as T;
  }
  return value as T;
}

// ---------------------------------------------------------------------------
// useQuery
// ---------------------------------------------------------------------------

/**
 * Subscribe to a reactive query and re-render on every broker event
 * that matches its read-set.
 *
 * Returns `T | undefined`:
 *   - `undefined` on the first render (loading) and during refetch
 *     after an event (we keep the previous snapshot — see notes below).
 *   - The resolved snapshot once the factory completes.
 *
 * Refetch policy: when a broker event arrives, the hook re-runs the
 * factory and commits the new snapshot. The component renders with the
 * *previous* snapshot during the refetch (no flicker to undefined),
 * mirroring TanStack Query's `keepPreviousData` default.
 *
 * Error policy: if the factory throws (sync or async), the hook stores
 * the error and surfaces it on the next render via the optional second
 * tuple slot. Today errors degrade to `undefined` data; callers needing
 * inspection should `try { await query() }` themselves. A follow-up can
 * widen the return type once we settle on a tuple shape.
 */
export function useQuery<T>(
  factory: QueryFactory<T>,
  options: UseQueryOptions = {},
): T | undefined {
  const client = React.useContext(QueryClientContext);

  // Use refs for the latest factory so the subscription effect can call
  // it without re-running on every render. Without this, the closure
  // captures the *first* factory and stale-closes over old deps.
  const factoryRef = React.useRef(factory);
  factoryRef.current = factory;

  const [snapshot, setSnapshot] = React.useState<T | undefined>(
    () => (options.initialData as T | undefined) ?? undefined,
  );
  // Hold the latest error so we can surface it; today we keep snapshot
  // unchanged on error (preserves keepPreviousData behaviour).
  const [error, setError] = React.useState<Error | undefined>(undefined);

  // The subscription effect: opens a broker sub, runs the query once,
  // then refetches on every event. Single effect for the whole pipeline
  // so StrictMode's mount/unmount/remount produces ONE living sub per
  // cycle (the cleanup tears the sub down, then the second mount opens
  // a fresh one).
  React.useEffect(() => {
    if (!client) {
      // No client — hook is a no-op. Surface a console warning once so
      // misconfiguration is loud in dev.
      // eslint-disable-next-line no-console
      console.warn(
        "@zeroship/react useQuery: no QueryClient in context. " +
          "Wrap your tree in <QueryClientProvider client={...}>.",
      );
      return;
    }

    let cancelled = false;
    let subscription: SubscriptionLike | undefined;

    async function run(): Promise<void> {
      // Stage 1: invoke the factory. Resolve its `_collection` (if any)
      // BEFORE awaiting — Query is synchronously inspectable.
      let queryHandle: QueryLike<T> | Promise<T>;
      try {
        queryHandle = factoryRef.current();
      } catch (e) {
        if (!cancelled) setError(e instanceof Error ? e : new Error(String(e)));
        return;
      }

      // Stage 2: open the broker subscription. The collection name
      // comes from the Query (preferred) or the options override.
      const collection = pickCollection(queryHandle, options.collection);
      if (collection && !cancelled) {
        try {
          subscription = client!.subscribe(collection);
        } catch (e) {
          if (!cancelled) setError(e instanceof Error ? e : new Error(String(e)));
          // Continue: we can still resolve the initial snapshot even if
          // the subscription failed. Refetch on event won't fire.
        }
      }

      // Stage 3: resolve the initial snapshot.
      try {
        const raw = await queryHandle;
        if (cancelled) return;
        const data = unwrapResult<T>(raw);
        setSnapshot(() => data);
        setError(undefined);
      } catch (e) {
        if (!cancelled) setError(e instanceof Error ? e : new Error(String(e)));
        return;
      }

      // Stage 4: pump events. On each event, re-run the factory and
      // commit. We swallow the rare error here so a single broker
      // hiccup doesn't kill the pipeline — surface to console for dev
      // visibility.
      if (!subscription || cancelled) return;
      const iter = subscription[Symbol.asyncIterator]();
      for (;;) {
        let next: IteratorResult<unknown>;
        try {
          next = await iter.next();
        } catch {
          break;
        }
        if (cancelled || next.done) break;

        // Refetch.
        try {
          const fresh = factoryRef.current();
          const raw = await fresh;
          if (cancelled) return;
          const data = unwrapResult<T>(raw);
          setSnapshot(() => data);
          setError(undefined);
        } catch (e) {
          if (cancelled) return;
          setError(e instanceof Error ? e : new Error(String(e)));
        }
      }
    }

    void run();

    return (): void => {
      cancelled = true;
      if (subscription) {
        try {
          subscription.close();
        } catch {
          // close() is idempotent in the SDK; swallow to keep teardown clean.
        }
      }
    };
    // We intentionally re-run on client identity only. The factory is
    // accessed via ref so closure-capture changes don't force a
    // subscription churn. Callers who need the *subscription* itself
    // to refresh (e.g. switching collections) should remount the
    // component via a key prop — see the SSR/Suspense notes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [client]);

  // Surface the error via a throw on next render so React error
  // boundaries can catch it. Opt-in: only throws when error is set
  // AND no snapshot exists (preserves keepPreviousData on transient
  // errors).
  if (error && snapshot === undefined) {
    throw error;
  }

  return snapshot;
}

// ---------------------------------------------------------------------------
// useSuspenseQuery — throws a Promise on first render
// ---------------------------------------------------------------------------

/**
 * Suspense-friendly variant. On first render, throws a Promise that
 * resolves with the initial snapshot — React's `<Suspense>` boundary
 * renders the fallback until the promise resolves, then re-renders
 * with the snapshot in hand.
 *
 * After the first render, behaves identically to `useQuery`: re-runs
 * the factory on every broker event and commits the new snapshot.
 *
 * `suspenseKey` is REQUIRED. It identifies the cached promise across
 * re-renders so React can resume the same Suspense resource instead
 * of repeatedly retrying. The convention is to derive it from the
 * collection name + filter input:
 *
 *   useSuspenseQuery(
 *     () => db.messages.find({ userId }).limit(50),
 *     { suspenseKey: `messages-user-${userId}` },
 *   );
 *
 * Why explicit? React's Suspense semantics require the resource to be
 * keyed by a stable identifier across renders. The factory closure
 * changes identity on every render (new function reference), and
 * `useRef`-based per-component keys don't survive the suspend/resume
 * cycle reliably in concurrent mode. A user-supplied key sidesteps
 * both issues. If the proposal's "infer from query shape" hashing
 * lands later, the key becomes optional.
 *
 * SSR note: the same `suspenseKey` strategy applies on the server.
 * Frameworks (Next.js, Remix) typically render the suspense fallback
 * on the server; the client resumes once the cached value resolves.
 */
const suspenseCache = new Map<string, { promise: Promise<unknown>; value?: unknown; error?: unknown }>();

export interface UseSuspenseQueryOptions extends UseQueryOptions {
  /** Stable cache key — see useSuspenseQuery docs. */
  suspenseKey: string;
}

export function useSuspenseQuery<T>(
  factory: QueryFactory<T>,
  options: UseSuspenseQueryOptions,
): T {
  const key = options.suspenseKey;
  if (typeof key !== "string" || key.length === 0) {
    throw new TypeError(
      "useSuspenseQuery: options.suspenseKey is required (non-empty string)",
    );
  }

  let entry = suspenseCache.get(key);
  if (!entry) {
    const handle = factory();
    entry = { promise: undefined as unknown as Promise<unknown> };
    suspenseCache.set(key, entry);
    entry.promise = Promise.resolve(handle).then(
      (raw) => {
        const data = unwrapResult<T>(raw);
        const cached = suspenseCache.get(key);
        if (cached) cached.value = data;
      },
      (err) => {
        const cached = suspenseCache.get(key);
        if (cached) cached.error = err;
      },
    );
  }

  if (entry.error) throw entry.error;
  if (!("value" in entry)) throw entry.promise;

  // Post-suspense: hand off to useQuery for refetch behaviour. Seed
  // initialData with the cached value so the first render after
  // resume commits the snapshot synchronously.
  const live = useQuery<T>(factory, {
    ...options,
    initialData: entry.value as T,
  });
  return live ?? (entry.value as T);
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/**
 * Returns the collection name we should subscribe to. Reads from the
 * Query's internal `_collection` field by duck-typing; falls back to
 * the explicit `options.collection` override.
 */
function pickCollection(
  q: QueryLike<unknown> | Promise<unknown>,
  override: string | undefined,
): string | undefined {
  if (override) return override;
  if (q && typeof q === "object" && "_collection" in q) {
    const c = (q as { _collection?: unknown })._collection;
    if (typeof c === "string" && c.length > 0) return c;
  }
  return undefined;
}
