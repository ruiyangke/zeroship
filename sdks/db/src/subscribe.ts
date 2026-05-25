/**
 * Reactive-query subscription primitive — P8a of the C1 reactive
 * queries phase of the @zeroship/db proposal.
 *
 * P8a-scope: coarse-grained, in-process. A subscription on
 * `collection` fires for every change to that collection by
 * subscribers in the same isolate. Read-set narrowing (P8b) +
 * cross-worker WAL fanout (P8a.2) + React `useQuery` (P8b) are
 * deferred.
 *
 * The native surface is the Subscription v8_class (returned from
 * `env.db.<collection>.openSubscription()`) with `.next()` →
 * Promise<SubscriptionEvent | null> and `.close()`. This module
 * wraps it into an `AsyncIterable` so callers can write:
 *
 * ```ts
 * for await (const ev of db.subscribe("messages")) {
 *   // ev.kind === "change" | "resync" | "closed"
 * }
 * ```
 *
 * The iterator terminates on the first `closed` event. If the
 * consumer breaks out of the `for await` loop early (or throws),
 * the iterator's `return` method runs `.close()` on the wrapper to
 * release the broker slot — same semantics as `EventEmitter`-backed
 * AsyncIterables in Node. The wrapper's GC finalizer is a safety net
 * for the case where the user drops every reference without
 * iterating to completion.
 */
import {
  nativeDbFromEnv,
  requireBoundNativeCapability,
  requireCollectionResolver,
  type NativeCollection,
  type NativeSubscriptionLike,
} from "./native.js";

/** The shape of one event surfaced to a subscriber. */
export type SubscriptionEvent =
  | {
      kind: "change";
      op: "insert" | "update" | "delete";
      collection: string;
      /** Surrogate primary key of the affected row, or null. */
      pk: string | null;
      /** Columns touched by the mutation (post-image, minus system fields). */
      columns: string[];
    }
  | {
      /** Bounded queue overflowed; client must re-fetch. */
      kind: "resync";
    }
  | {
      /** Subscription closed; iterator terminates. */
      kind: "closed";
    };

type _Assert<T extends true> = T;
type _IsExactly<A, B> = (
  (<T>() => T extends A ? 1 : 2) extends (<T>() => T extends B ? 1 : 2) ? true : false
);
type _SubscriptionEventPkMatchesRuntime = _Assert<
  _IsExactly<Extract<SubscriptionEvent, { kind: "change" }>["pk"], string | null>
>;
type _SubscriptionEventPkMatchesPublished = _Assert<
  _IsExactly<Extract<ZeroshipSubscriptionEvent, { kind: "change" }>["pk"], string | null>
>;

/**
 * A live subscription — an `AsyncIterable<SubscriptionEvent>` that
 * additionally exposes `close()` for explicit teardown.
 *
 * Lifetime: the underlying broker slot is held until either:
 * - the iterator drains a `closed` event (auto-reaped), OR
 * - `close()` is called, OR
 * - the iterator's `return()` is invoked (e.g. via `for await`
 *   `break`), OR
 * - the wrapper is garbage-collected (Subscription v8_class GC
 *   finalizer is the safety-net release).
 */
export interface Subscription extends AsyncIterable<SubscriptionEvent> {
  /** Idempotent close. Subsequent iterator polls resolve with
   *  `{kind:"closed"}` and the iterator terminates. */
  close(): void;
}

type NativeSubscription = NativeSubscriptionLike<SubscriptionEvent>;

/** Pull the native handle off `env`, throwing on a misconfigured runtime. */
function getNativeDbCollection(name: string): NativeCollection {
  const db = nativeDbFromEnv();
  return requireCollectionResolver(db, {
    code: "NATIVE_SUBSCRIPTION_UNAVAILABLE",
    message:
      "@zeroship/db/subscribe: env.db.collection not available — " +
      "runtime is missing the Db v8_class surface.",
  })(name);
}

/**
 * Open a subscription on `collection`. Returns an
 * `AsyncIterable<SubscriptionEvent>` that yields one event per
 * `next()` call.
 */
export function subscribe(collection: string): Subscription {
  if (typeof collection !== "string" || collection.length === 0) {
    throw Object.assign(
      new TypeError(
        "@zeroship/db/subscribe: collection must be a non-empty string",
      ),
      { code: "SUBSCRIBE_INVALID_COLLECTION" as const },
    );
  }
  const col = getNativeDbCollection(collection);
  const openSubscription = requireBoundNativeCapability(
    col,
    "openSubscription",
    {
      code: "NATIVE_SUBSCRIPTION_UNAVAILABLE",
      message:
        "@zeroship/db/subscribe: env.db.<collection>.openSubscription not available — " +
        "runtime is missing the Subscription v8_class surface.",
    },
  );
  const sub = openSubscription();
  let closed = false;

  function doClose(): void {
    if (closed) return;
    closed = true;
    try {
      sub.close();
    } catch {
      // Idempotent — the native side may already have reaped the
      // wrapper if the iterator drained a `closed` event.
    }
  }

  const iter: AsyncIterator<SubscriptionEvent> = {
    async next(): Promise<IteratorResult<SubscriptionEvent>> {
      if (closed) {
        return { value: undefined, done: true };
      }
      const parsed = await sub.next();
      if (parsed === null) {
        // Wrapper is closed — equivalent to a closed event we missed.
        closed = true;
        return { value: undefined, done: true };
      }
      if (parsed.kind === "closed") {
        closed = true;
      }
      return { value: parsed, done: false };
    },
    async return(value): Promise<IteratorResult<SubscriptionEvent>> {
      doClose();
      return { value, done: true } as IteratorResult<SubscriptionEvent>;
    },
    async throw(err): Promise<IteratorResult<SubscriptionEvent>> {
      doClose();
      throw err;
    },
  };

  return {
    close: doClose,
    [Symbol.asyncIterator](): AsyncIterator<SubscriptionEvent> {
      return iter;
    },
  };
}
