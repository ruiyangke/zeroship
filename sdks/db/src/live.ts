/**
 * `db.live(queryFn)` — reactive query layer over the coarse-grained
 * per-table subscription broker.
 *
 * Today `env.db.openSubscription(name)` emits one event per row mutation
 * on a collection. App code that wants a *query-shaped* live primitive
 * (initial result + a fresh result on every relevant change) has to wire
 * the refetch + diff loop itself. `db.live(queryFn)` does that wiring:
 *
 *   const live = db.live(() => db.todos.find({ userId }));
 *   for await (const todos of live) { ... }  // initial + every change
 *   live.close();                            // explicit teardown
 *
 * v1 scope (intentionally small):
 *
 *   - Table-set detection: while the first `queryFn()` runs, every
 *     `Collection.find/findOne/get/...` call pushes the collection name
 *     into a module-level tracking context (`liveTracker.current`).
 *     The set is frozen after the first execution — subsequent reruns
 *     do NOT re-observe (a queryFn that conditionally touches different
 *     tables would need the explicit `{tables}` escape).
 *
 *   - Re-execution: every event from any of the watched tables triggers
 *     a fresh `queryFn()` invocation; the new result array is pushed
 *     into the AsyncIterable's queue. v1 yields every rerun; deep-equal
 *     diffing to suppress no-op events is future work.
 *
 *   - Tx awareness: calling `db.live` inside a `db.transaction(tx => ...)`
 *     callback throws synchronously with `code = "live_in_transaction"`.
 *     Live queries are by definition long-lived; a tx is per-request.
 *
 * Limitations (future work, not v1):
 *
 *   - Coarse-grained: any insert/update/delete on a watched table fires
 *     a rerun, even if the row doesn't match the queryFn's filter.
 *   - No deep-equal diffing: every rerun yields, even if the result is
 *     identical to the previous one.
 *   - No read-set narrowing (Convex-style per-document subscription).
 *   - If `queryFn` returns a raw `Promise<R[]>` that doesn't touch any
 *     Collection method, table detection finds nothing — the caller
 *     MUST pass `db.live(queryFn, { tables: ["todos"] })` explicitly.
 */

import { subscribe, type Subscription, type SubscriptionEvent } from "./subscribe.js";

/**
 * The minimal contract a `queryFn` return value must satisfy. Either a
 * raw `Promise<R[]>` or any thenable (e.g. the `Query` builder, which
 * resolves to `Result<R[]>` — `db.live` unwraps the Result below).
 */
type QueryFnResult<R> = Promise<R[]> | { then(onFulfilled: (value: unknown) => unknown, onRejected?: (reason: unknown) => unknown): unknown };

/**
 * Module-scope tracking context. While `current` is non-null, every
 * Collection read method pushes its `name` into `current.collections`.
 * `current` is a *stack* discipline: nested `db.live(() => db.live(...))`
 * (rare but possible if a queryFn opens another live query) saves +
 * restores the previous value, so the outer context doesn't lose
 * track of tables touched outside the inner call.
 */
const liveTracker: { current: { collections: Set<string> } | null } = { current: null };

/** Push a collection name into the active tracker, if one is set. */
export function trackCollectionAccess(name: string): void {
  if (liveTracker.current !== null) {
    liveTracker.current.collections.add(name);
  }
}

/** @internal — test-only handle. */
export function __zeroshipLiveTrackerCurrentForTest(): { collections: Set<string> } | null {
  return liveTracker.current;
}

/** Options for `db.live`. */
export interface LiveOptions {
  /**
   * Explicit list of tables to subscribe to. When provided, the
   * Collection-method auto-tracking is bypassed — useful if `queryFn`
   * returns a raw `Promise<R[]>` that doesn't go through a Collection
   * (e.g. fetch from an external service, then transform).
   */
  tables?: string[];
}

/**
 * The handle returned by `db.live`. AsyncIterable so callers write
 * `for await (const rows of live) ...`; `close()` is an explicit
 * teardown channel.
 */
export interface LiveQuery<R> extends AsyncIterableIterator<R[]> {
  /** Idempotent. Cancels every underlying subscription and resolves
   *  any pending `next()` with `{done: true}`. */
  close(): void;
}

/**
 * Set of collections that have `_txDepth > 0`. Reading any of them is
 * the "inside an active transaction" signal — `db.live` rejects up
 * front rather than registering a subscription that would race the
 * tx commit.
 */
type CollectionLike = { _txDepth?: number };

function anyCollectionInTx(db: Record<string, unknown>): boolean {
  for (const v of Object.values(db)) {
    if (v === null || typeof v !== "object") continue;
    const depth = (v as CollectionLike)._txDepth;
    if (typeof depth === "number" && depth > 0) return true;
  }
  return false;
}

/**
 * Unwrap whatever `queryFn` returned into a `Promise<R[]>`. A Query
 * builder resolves to `Result<R[]>` (`{data, error}`); a bare Promise
 * resolves to `R[]` directly. We detect the Result shape by structural
 * test on the resolved value.
 */
function unwrapQueryFnResult<R>(value: QueryFnResult<R>): Promise<R[]> {
  return Promise.resolve(value as unknown as Promise<unknown>).then((resolved) => {
    if (resolved !== null && typeof resolved === "object" && "data" in (resolved as object) && "error" in (resolved as object)) {
      const r = resolved as { data: R[] | null; error: Error | null };
      if (r.error) throw r.error;
      return (r.data ?? []) as R[];
    }
    return resolved as R[];
  });
}

/**
 * Build a `LiveQuery<R>` from a `queryFn` and the parent `db` object.
 * Exported via `db.live` (see `db.ts`). `db` is the object returned
 * by `createDb` — used to (a) verify we're not inside a tx and (b)
 * call `subscribe(name)` for every detected table.
 */
export function createLive<R>(
  db: Record<string, unknown>,
  queryFn: () => QueryFnResult<R>,
  options?: LiveOptions,
): LiveQuery<R> {
  if (anyCollectionInTx(db)) {
    throw Object.assign(
      new Error("@zeroship/db: db.live cannot be called inside db.transaction — live queries outlive the request-scoped tx"),
      { code: "live_in_transaction" as const },
    );
  }

  // Queue of results waiting to be consumed by `next()`, plus a
  // single-slot pending resolver if a consumer is waiting. The queue
  // can also hold a terminal `done` sentinel (`null`) so `close()`
  // wakes up a pending `next()` cleanly.
  type QueueItem = { kind: "value"; value: R[] } | { kind: "error"; error: Error } | { kind: "done" };
  const queue: QueueItem[] = [];
  let pendingResolve: ((r: IteratorResult<R[]>) => void) | null = null;
  let pendingReject: ((e: unknown) => void) | null = null;
  let closed = false;
  const subscriptions: Subscription[] = [];

  function pump(item: QueueItem): void {
    if (pendingResolve !== null) {
      const resolve = pendingResolve;
      const reject = pendingReject;
      pendingResolve = null;
      pendingReject = null;
      if (item.kind === "value") {
        resolve({ value: item.value, done: false });
      } else if (item.kind === "done") {
        resolve({ value: undefined as unknown as R[], done: true });
      } else {
        if (reject) reject(item.error);
        else resolve({ value: undefined as unknown as R[], done: true });
      }
      return;
    }
    queue.push(item);
  }

  function doClose(): void {
    if (closed) return;
    closed = true;
    for (const sub of subscriptions) {
      try { sub.close(); } catch { /* idempotent */ }
    }
    subscriptions.length = 0;
    pump({ kind: "done" });
  }

  // Run `queryFn` with the tracker installed if no explicit tables were
  // given. Stack-save the previous tracker so nested `db.live` calls
  // compose. We return both the first-result promise and the table set
  // so the broker wiring can start as soon as we have the names.
  async function firstRun(): Promise<{ rows: R[]; tables: Set<string> }> {
    if (options?.tables && options.tables.length > 0) {
      const rows = await unwrapQueryFnResult<R>(queryFn());
      return { rows, tables: new Set(options.tables) };
    }
    const prev = liveTracker.current;
    const ctx = { collections: new Set<string>() };
    liveTracker.current = ctx;
    try {
      const rows = await unwrapQueryFnResult<R>(queryFn());
      return { rows, tables: ctx.collections };
    } finally {
      liveTracker.current = prev;
    }
  }

  // After the table set is fixed, every subsequent rerun must NOT
  // re-tracker: a queryFn that touches different tables on subsequent
  // runs would otherwise mutate the watched-set in a way the broker
  // can't catch up with. The explicit `{tables}` escape covers that case.
  async function rerun(): Promise<void> {
    if (closed) return;
    try {
      const rows = await unwrapQueryFnResult<R>(queryFn());
      if (closed) return;
      pump({ kind: "value", value: rows });
    } catch (e) {
      if (closed) return;
      pump({ kind: "error", error: e instanceof Error ? e : new Error(String(e)) });
    }
  }

  // Drive each subscription on a background async task. The native
  // wrapper's `next()` resolves one event at a time; we loop until it
  // signals `closed` (or our own `doClose` runs).
  function driveSubscription(sub: Subscription): void {
    void (async () => {
      const iter = sub[Symbol.asyncIterator]();
      while (!closed) {
        let ev: IteratorResult<SubscriptionEvent>;
        try {
          ev = await iter.next();
        } catch {
          break;
        }
        if (ev.done) break;
        if (ev.value.kind === "closed") break;
        // Both `change` and `resync` trigger a rerun. A resync means
        // the broker dropped events; the safest response is a full
        // refetch, which is exactly what `rerun()` already does.
        await rerun();
      }
    })();
  }

  // Kick off the first run; on success, yield the initial result and
  // open one subscription per detected table. On failure, surface the
  // error via the iterator's next() and terminate.
  void (async () => {
    try {
      const { rows, tables } = await firstRun();
      if (closed) return;
      pump({ kind: "value", value: rows });
      for (const name of tables) {
        if (closed) break;
        const sub = subscribe(name);
        subscriptions.push(sub);
        driveSubscription(sub);
      }
      // If no tables were detected, the live query is effectively
      // static — it yields the initial result and then stalls. We
      // don't auto-close because the caller may still call `close()`
      // explicitly; surfacing the empty-tables case as a one-shot
      // result matches the behaviour of `for await (const x of [v])`.
    } catch (e) {
      if (closed) return;
      pump({ kind: "error", error: e instanceof Error ? e : new Error(String(e)) });
      doClose();
    }
  })();

  const iter: LiveQuery<R> = {
    async next(): Promise<IteratorResult<R[]>> {
      if (queue.length > 0) {
        const item = queue.shift()!;
        if (item.kind === "value") return { value: item.value, done: false };
        if (item.kind === "done") return { value: undefined as unknown as R[], done: true };
        throw item.error;
      }
      if (closed) return { value: undefined as unknown as R[], done: true };
      return new Promise<IteratorResult<R[]>>((resolve, reject) => {
        pendingResolve = resolve;
        pendingReject = reject;
      });
    },
    async return(value): Promise<IteratorResult<R[]>> {
      doClose();
      return { value: value as R[], done: true };
    },
    async throw(err): Promise<IteratorResult<R[]>> {
      doClose();
      throw err;
    },
    close: doClose,
    [Symbol.asyncIterator](): AsyncIterableIterator<R[]> {
      return iter;
    },
  };
  return iter;
}
