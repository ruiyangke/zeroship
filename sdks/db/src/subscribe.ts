/**
 * Reactive-query subscription primitive — P8a of the C1 reactive
 * queries phase of the @zeroship/db v2 proposal.
 *
 * P8a-scope: coarse-grained, in-process. A subscription on
 * `collection` fires for every change to that collection by
 * subscribers in the same isolate. Read-set narrowing (P8b) +
 * cross-worker WAL fanout (P8a.2) + React `useQuery` (P8b) are
 * deferred.
 *
 * The native surface is three primitives (`subscribe`,
 * `subscribePoll`, `subscribeClose`) — see `sdks/types/db.d.ts`.
 * This module wraps them into an `AsyncIterable` so callers can
 * write:
 *
 * ```ts
 * for await (const ev of db.subscribe("messages")) {
 *   // ev.kind === "change" | "resync" | "closed"
 * }
 * ```
 *
 * The iterator terminates on the first `closed` event. If the
 * consumer breaks out of the `for await` loop early (or throws),
 * the iterator's `return` method runs `subscribeClose(handle)`
 * to release the broker slot — same semantics as
 * `EventEmitter`-backed AsyncIterables in Node.
 */

import { env } from "zeroship";

/** The shape of one event surfaced to a subscriber. */
export type SubscriptionEvent =
  | {
      kind: "change";
      op: "insert" | "update" | "delete";
      collection: string;
      /** Surrogate primary key of the affected row, or null. */
      pk: number | null;
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

/**
 * A live subscription — an `AsyncIterable<SubscriptionEvent>` that
 * additionally exposes `close()` for explicit teardown and `handle`
 * for diagnostics.
 *
 * Lifetime: the underlying broker slot is held until either:
 * - the iterator drains a `closed` event (auto-reaped), OR
 * - `close()` is called, OR
 * - the iterator's `return()` is invoked (e.g. via `for await`
 *   `break`).
 */
export interface Subscription extends AsyncIterable<SubscriptionEvent> {
  /** Numeric handle the broker uses internally. Stable for the
   *  lifetime of the subscription. Exposed for debugging only. */
  readonly handle: number;
  /** Idempotent close. Subsequent iterator polls resolve with
   *  `{kind:"closed"}` and the iterator terminates. */
  close(): void;
}

/** The native zeroship.db handle resolved off the runtime env. */
type NativeDb = {
  subscribe: (collection: string) => number;
  subscribePoll: (handle: number) => Promise<string | null>;
  subscribeClose: (handle: number) => void;
};

/** Pull the native handle off `env`, throwing on a misconfigured runtime. */
function getNativeDb(): NativeDb {
  const db = (env as { db?: NativeDb } | undefined)?.db;
  if (!db || typeof db.subscribe !== "function") {
    throw new Error(
      "@zeroship/db/subscribe: env.db is not available — " +
        "is the DbPlugin registered on this runtime?",
    );
  }
  return db;
}

/**
 * Open a subscription on `collection`. Returns an
 * `AsyncIterable<SubscriptionEvent>` that yields one event per
 * `next()` call.
 *
 * Errors:
 * - throws if `env.db` isn't available (plugin not registered)
 * - throws if the underlying poll resolves with malformed JSON
 *   (should not happen in practice — the native layer always
 *   emits well-formed objects)
 *
 * Example:
 *
 * ```ts
 * import { subscribe } from "@zeroship/db";
 *
 * const sub = subscribe("messages");
 * try {
 *   for await (const ev of sub) {
 *     if (ev.kind === "change") {
 *       console.log("messages changed:", ev.op, ev.pk);
 *     } else if (ev.kind === "resync") {
 *       await refetchAll();
 *     }
 *   }
 * } finally {
 *   sub.close();
 * }
 * ```
 */
export function subscribe(collection: string): Subscription {
  if (typeof collection !== "string" || collection.length === 0) {
    throw new TypeError(
      "@zeroship/db/subscribe: collection must be a non-empty string",
    );
  }
  const native = getNativeDb();
  const handle = native.subscribe(collection);
  let closed = false;

  function doClose(): void {
    if (closed) return;
    closed = true;
    try {
      native.subscribeClose(handle);
    } catch {
      // Idempotent — the native side may already have reaped the
      // handle if the iterator drained a `closed` event.
    }
  }

  const iter: AsyncIterator<SubscriptionEvent> = {
    async next(): Promise<IteratorResult<SubscriptionEvent>> {
      if (closed) {
        return { value: undefined, done: true };
      }
      const raw = await native.subscribePoll(handle);
      if (raw === null) {
        // Handle is gone — equivalent to a closed event we missed.
        closed = true;
        return { value: undefined, done: true };
      }
      let parsed: SubscriptionEvent;
      try {
        parsed = JSON.parse(raw) as SubscriptionEvent;
      } catch (e) {
        // Should not happen — defensive.
        closed = true;
        throw new Error(
          `@zeroship/db/subscribe: malformed event JSON from native: ${String(e)}`,
        );
      }
      if (parsed.kind === "closed") {
        closed = true;
        return { value: parsed, done: false };
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
    handle,
    close: doClose,
    [Symbol.asyncIterator](): AsyncIterator<SubscriptionEvent> {
      return iter;
    },
  };
}
