/** Coalesce key lookups while preserving the transaction scope at enqueue time. */

import type { IdValue } from "./types.js";
import { identityKey } from "./identity.js";

interface QueuedLoad<R, K extends IdValue> {
  id: K;
  resolve: (row: R | null) => void;
  reject: (err: unknown) => void;
  txDepthAtEnqueue: number;
}

import { MAX_ID_BATCH } from "./membership-cap.js";

/**
 * Coalesces `.load(id)` calls within a microtask into a single batched
 * fetch. Construct one per Collection and reuse — it's stateless across
 * batches.
 */
export class IdLoader<R, K extends IdValue = string> {
  private queue: QueuedLoad<R, K>[] = [];
  private scheduled = false;

  /**
   * @param flush       Called with the deduped id list when the microtask
   *                    fires. Must resolve a `Map<id, row>`; ids without
   *                    a matching row are reported back as `null`.
   * @param getTxDepth  Returns the current tx-depth on the owning
   *                    Collection. Used both at enqueue time (snapshot
   *                    via `load(id)`'s second argument) and at flush
   *                    time so we can detect a tx opening mid-batch.
   */
  constructor(
    private flush: (ids: K[]) => Promise<Map<K, R>>,
    private getTxDepth: () => number = () => 0,
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
    return new Promise<R | null>((resolve, reject) => {
      this.queue.push({ id, resolve, reject, txDepthAtEnqueue: txDepthSnapshot });
      if (!this.scheduled) {
        this.scheduled = true;
        queueMicrotask(() => this.dispatch());
      }
    });
  }

  /** @internal — flush queued requests now. Used by tests and to clear
   *  pending work at transaction boundaries. */
  async _drain(): Promise<void> {
    if (this.queue.length === 0) {
      this.scheduled = false;
      return;
    }
    await this.dispatch();
  }

  private async dispatch(): Promise<void> {
    const batch = this.queue;
    this.queue = [];
    this.scheduled = false;
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
