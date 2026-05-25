/**
 * `db.live(queryFn)` — reactive query layer over the coarse-grained
 * per-table subscription broker.
 *
 * Today `env.db.<collection>.openSubscription()` emits one event per
 * row mutation on a collection. App code that wants a *query-shaped*
 * live primitive
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
 *     `Collection.find/get/...` call pushes the collection name
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
 *     callback throws synchronously with `code = "LIVE_IN_TRANSACTION"`.
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

/**
 * Module-scope latch — `console.warn` once per process when a caller
 * passes `tables: []`. The empty list is treated as "subscribe to
 * nothing" (a one-shot static query), but it usually indicates a typo
 * or stale state where the caller meant to provide actual table names.
 */
let _warnedEmptyTables = false;

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
 * resolves to `R[]` directly. We detect the Result shape strictly: the
 * resolved object must have EXACTLY the keys `data` and `error` (length
 * 2, no extras). The loose `"data" in obj && "error" in obj` check
 * false-positives on user rows that happen to include both columns —
 * e.g. `db.live(() => [{data: 1, error: null}])` would be unwrapped to
 * the value of `data` instead of preserved verbatim.
 */
function unwrapQueryFnResult<R>(value: QueryFnResult<R>): Promise<R[]> {
  return Promise.resolve(value as unknown as Promise<unknown>).then((resolved) => {
    if (isResultEnvelope(resolved)) {
      const r = resolved as { data: R[] | null; error: Error | null };
      if (r.error) throw r.error;
      return (r.data ?? []) as R[];
    }
    return resolved as R[];
  });
}

/** Strict shape test for the `Result<T>` envelope. The object must have
 *  exactly two own enumerable keys, both named `data` and `error`. This
 *  avoids the false positive on rows whose payload happens to include
 *  both keys among others (e.g. an event log row). */
function isResultEnvelope(v: unknown): v is { data: unknown; error: unknown } {
  if (v === null || typeof v !== "object") return false;
  const keys = Object.keys(v as object);
  return keys.length === 2 && keys.includes("data") && keys.includes("error");
}

/**
 * Build a `LiveQuery<R>` from a `queryFn` and the parent `db` object.
 * Exported via `env.db.live` (planted by `installSchema` — see
 * `db.ts`). `db` is the Collections map `installSchema` built —
 * used to (a) verify we're not inside a tx and (b) call
 * `subscribe(name)` for every detected table.
 */
export function createLive<R>(
  db: Record<string, unknown>,
  queryFn: () => QueryFnResult<R>,
  options?: LiveOptions,
): LiveQuery<R> {
  if (anyCollectionInTx(db)) {
    throw Object.assign(
      new Error("@zeroship/db: db.live cannot be called inside db.transaction — live queries outlive the request-scoped tx"),
      { code: "LIVE_IN_TRANSACTION" as const },
    );
  }

  // Queue of results waiting to be consumed by `next()`, plus a
  // single-slot pending resolver if a consumer is waiting. The queue
  // can also hold a terminal `done` sentinel (`null`) so `close()`
  // wakes up a pending `next()` cleanly.
  //
  // Bounded at MAX_QUEUE_DEPTH to keep producers from outrunning a slow
  // consumer indefinitely. On overflow we drop the OLDEST value entry
  // (never the newest, never a `done`/`error` sentinel) — the freshest
  // result is what callers care about for a "current snapshot" reactive
  // view, and any consumer that's lagging can only act on the latest
  // anyway. Matches the broker's overflow semantics (newest wins).
  type QueueItem = { kind: "value"; value: R[] } | { kind: "error"; error: Error } | { kind: "done" };
  type PendingConsumer = {
    resolve: (r: IteratorResult<R[]>) => void;
    reject: (e: unknown) => void;
  };
  const MAX_QUEUE_DEPTH = 64;
  const queue: QueueItem[] = [];
  // FIFO of awaiting `next()` consumers. Most users iterate single-in-
  // flight (`for await`), so this queue is at most length 1; but the
  // `AsyncIterableIterator` contract is that `next()` queues, and racing
  // consumers (`Promise.race([iter.next(), timer])`,
  // `Promise.all([iter.next(), iter.next()])`) drop into the second slot.
  // A single-slot pending resolver silently leaks the first promise.
  const pendingConsumers: PendingConsumer[] = [];
  let closed = false;
  const subscriptions: Subscription[] = [];

  function pump(item: QueueItem): void {
    if (pendingConsumers.length > 0) {
      const { resolve, reject } = pendingConsumers.shift()!;
      if (item.kind === "value") {
        resolve({ value: item.value, done: false });
      } else if (item.kind === "done") {
        resolve({ value: undefined as unknown as R[], done: true });
        // Done is terminal — wake every other waiter too.
        while (pendingConsumers.length > 0) {
          const next = pendingConsumers.shift()!;
          next.resolve({ value: undefined as unknown as R[], done: true });
        }
      } else {
        reject(item.error);
        // An error event is one-shot from the producer's perspective —
        // the failing `rerun()` doesn't loop, it pushes ONE error item.
        // If multiple consumers are waiting (`Promise.all([iter.next(),
        // iter.next()])`), shifting + rejecting one leaves the others
        // hanging forever. Reject every pending consumer with the same
        // error so racing callers all observe the failure. (Matches the
        // `done` branch's drain semantics for the failure case.)
        while (pendingConsumers.length > 0) {
          const next = pendingConsumers.shift()!;
          next.reject(item.error);
        }
      }
      return;
    }
    // Once closed, additional items dropped on the floor — the `next()`
    // fast-path returns `{done: true}` directly when `closed && queue
    // empty`, so a queued done sentinel here is dead weight. (R3
    // IMPORTANT-2: a post-error `pump({kind:"done"})` in doClose used
    // to land in the queue with no consumer, never delivered.)
    if (closed) return;
    if (queue.length >= MAX_QUEUE_DEPTH) {
      // Drop the oldest VALUE entry to make room. Preserve any errors
      // or the done sentinel so close/failure signals always propagate.
      const dropIdx = queue.findIndex((q) => q.kind === "value");
      if (dropIdx >= 0) queue.splice(dropIdx, 1);
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
    // Drain any pending consumers with `{done: true}`. The `pump` will
    // no-op on the queue path because `closed` is now true; the
    // pendingConsumer drain branch is what we want here.
    pump({ kind: "done" });
  }

  // Run `queryFn` with the tracker installed if no explicit tables were
  // given. Stack-save the previous tracker so nested `db.live` calls
  // compose. We return both the first-result promise and the table set
  // so the broker wiring can start as soon as we have the names.
  async function firstRun(): Promise<{ rows: R[]; tables: Set<string> }> {
    // Explicit `tables` array is authoritative — including `[]`, which
    // means "subscribe to nothing" (static one-shot result). A user who
    // wants auto-tracking omits `tables` entirely, not passes `[]`.
    // Warn once per process so an accidental empty array surfaces.
    if (options?.tables !== undefined) {
      if (options.tables.length === 0 && !_warnedEmptyTables) {
        _warnedEmptyTables = true;
        console.warn(
          "[@zeroship/db] db.live({ tables: [] }) treated as 'subscribe to nothing' — " +
          "the iterator will yield the initial result then stall. Omit `tables` to enable auto-tracking, " +
          "or pass actual table names.",
        );
      }
      // R4 IMPORTANT-2 — install a throwaway tracker for the duration
      // of `queryFn()` so Collection reads inside the explicit-tables
      // queryFn don't leak into an enclosing `db.live`'s watched set.
      // Pre-fix, an inner `db.live(qfn, { tables })` nested inside an
      // outer `db.live(() => ...)` would see the OUTER tracker still
      // installed; inner's reads would push into outer's collections
      // set, inflating outer's subscriptions and triggering spurious
      // reruns on every mutation of an inner-only table.
      const prev = liveTracker.current;
      const sink = { collections: new Set<string>() };
      liveTracker.current = sink;
      try {
        const rows = await unwrapQueryFnResult<R>(queryFn());
        return { rows, tables: new Set(options.tables) };
      } finally {
        liveTracker.current = prev;
      }
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
        pendingConsumers.push({ resolve, reject });
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
