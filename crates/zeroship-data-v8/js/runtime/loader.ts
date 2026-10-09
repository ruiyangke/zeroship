/** Coalesce key lookups while preserving the transaction scope at enqueue time. */

import type { IdValue } from "../../../../packages/db/src/types";
import { identityKey } from "./identity";

interface QueuedLoad<R, K extends IdValue> {
  id: K;
  resolve: (row: R | null) => void;
  reject: (err: unknown) => void;
  txDepthAtEnqueue: number;
}

import { MAX_ID_BATCH } from "./membership-cap";

/**
 * Names the invocation the calling code belongs to: an object the host hands
 * out per request, the same for every turn of that request, or `undefined`
 * for code that belongs to no request.
 *
 * A batch is flushed by one call, which the host runs as the request that
 * scheduled the flush. So calls are batched only with calls made for the same
 * request: a request's reads then run as that request, never as another one
 * that happened to schedule the flush, and a request that has ended cannot
 * take a live one's reads down with it, or have its own admitted under it.
 */
export type InvocationKey = () => object | undefined;

const SOLE_INVOCATION = {};

/**
 * The key where nothing tells requests apart: every call is taken as made for
 * the one request there is. A host that runs many requests passes its own.
 */
export const soleInvocation: InvocationKey = () => SOLE_INVOCATION;

/**
 * Coalesces `.load(id)` calls made for one request within a microtask into a
 * single batched fetch. Construct one per Collection and reuse.
 */
export class IdLoader<R, K extends IdValue = string> {
  /** One pending batch per request, in the order the requests queued them. */
  private batches = new Map<object, QueuedLoad<R, K>[]>();

  /**
   * @param flush         Called with the deduped id list when the microtask
   *                      fires. Must resolve a `Map<id, row>`; ids without
   *                      a matching row are reported back as `null`.
   * @param getTxDepth    Returns the current tx-depth on the owning
   *                      Collection. Used both at enqueue time (snapshot
   *                      via `load(id)`'s second argument) and at flush
   *                      time so we can detect a tx opening mid-batch.
   * @param invocationKey The request each call is made for. A call made for
   *                      no request is flushed on its own, never batched.
   */
  constructor(
    private flush: (ids: K[]) => Promise<Map<K, R>>,
    private getTxDepth: () => number = () => 0,
    private invocationKey: InvocationKey = soleInvocation,
  ) {}

  /**
   * Queue a single id-fetch. The returned promise resolves on the next
   * microtask after `flush` settles. `txDepthSnapshot` is the caller's
   * tx-depth at the synchronous call boundary of `get(id)` (read BEFORE
   * any `await` in the caller); if a tx opens between enqueue and flush
   * the entry will be rejected with a clear race error instead of being
   * silently routed onto the tx connection.
   */
  load(id: K, txDepthSnapshot: number = 0): Promise<R | null> {
    const key = this.invocationKey();
    return new Promise<R | null>((resolve, reject) => {
      const entry = { id, resolve, reject, txDepthAtEnqueue: txDepthSnapshot };
      if (key === undefined) {
        queueMicrotask(() => void this.dispatch([entry]));
        return;
      }
      const pending = this.batches.get(key);
      if (pending !== undefined) {
        pending.push(entry);
        return;
      }
      const batch = [entry];
      this.batches.set(key, batch);
      // Scheduled by this request's own call, so the flush runs as this
      // request. A batch the request drained early is gone by then.
      queueMicrotask(() => {
        if (this.batches.get(key) !== batch) return;
        this.batches.delete(key);
        void this.dispatch(batch);
      });
    });
  }

  /** @internal - flush the calling request's queued loads now. Used by tests
   *  and to clear pending work at transaction boundaries. */
  async _drain(): Promise<void> {
    const key = this.invocationKey();
    const batch = key === undefined ? undefined : this.batches.get(key);
    if (key === undefined || batch === undefined) return;
    this.batches.delete(key);
    await this.dispatch(batch);
  }

  private async dispatch(batch: QueuedLoad<R, K>[]): Promise<void> {
    if (batch.length === 0) return;

    // Tx-race split: entries enqueued outside a tx that now find a tx
    // active are rejected — the underlying `find` would otherwise route
    // through TX_CONN and silently leak into the tx scope. Entries
    // enqueued inside a tx (snapshot > 0) keep their original routing
    // intent: if the tx already ended, that's the caller's bug, not
    // ours.
    const currentTxDepth = this.getTxDepth();
    const liveBatch: QueuedLoad<R, K>[] = [];
    for (const q of batch) {
      if (q.txDepthAtEnqueue === 0 && currentTxDepth > 0) {
        q.reject(
          Object.assign(
            new Error(
              "DataLoader: batched read started outside a transaction but a " +
              "transaction opened before flush. await the get() before " +
              "db.transaction(...) to avoid this race.",
            ),
            { code: "LOADER_TX_RACE" as const },
          ),
        );
      } else {
        liveBatch.push(q);
      }
    }
    if (liveBatch.length === 0) return;

    // Dedupe ids before the underlying call — N concurrent `get("post_X")`
    // calls resolve from the same row without N copies on the wire.
    const ids: K[] = [];
    const seen = new Set<string>();
    for (const q of liveBatch) {
      if (!seen.has(identityKey(q.id))) {
        seen.add(identityKey(q.id));
        ids.push(q.id);
      }
    }

    try {
      // Chunked: the native builder REJECTS a membership list longer than the
      // cap rather than clamping it, so an unbounded batch fails EVERY queued
      // get() at once - not just the ids past the boundary. A microtask batch
      // is as large as the caller's concurrency, so `Promise.all` over a few
      // hundred distinct ids reaches it without anything unusual happening.
      //
      // Sequential, matching the relation loader: these chunks exist because
      // one call was already too big, and issuing them concurrently would put
      // the same total work in flight simultaneously.
      const map = new Map<string, R>();
      for (let i = 0; i < ids.length; i += MAX_ID_BATCH) {
        const part = await this.flush(ids.slice(i, i + MAX_ID_BATCH));
        for (const [k, v] of part) map.set(identityKey(k), v);
      }
      for (const q of liveBatch) resolve(q, map);
    } catch (e) {
      for (const q of liveBatch) q.reject(e);
    }
  }
}

function resolve<R, K extends IdValue>(
  q: QueuedLoad<R, K>,
  map: Map<string, R>,
): void {
  const row = map.get(identityKey(q.id));
  q.resolve(row === undefined ? null : row);
}
