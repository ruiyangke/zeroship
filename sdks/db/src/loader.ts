/**
 * Per-collection DataLoader for `get(id)`.
 *
 * Coalesces multiple `Collection.get(idOrFilter)` calls that fire within
 * one microtask into a single `WHERE id IN (...)` query. The pattern is
 * the cohort-standard for AI-builder runtimes (Prisma's `findUnique` is
 * now batched, Convex/Drizzle/GraphQL DataLoader popularised it).
 *
 * The loader is transparent: it lives behind the existing `get(id)` API
 * and emits exactly one underlying `find({id: {$in: [...]}})` per batch.
 * Errors from the underlying call propagate to every queued promise.
 *
 * Scope and limits are enforced by the caller (`Collection.get`):
 *   - typed_id string only (not a Filter object)
 *   - no `opts.select` (would force per-projection bucketing)
 *   - no `opts.orderBy` (irrelevant for id reads; falls through to be safe)
 *   - skipped while a transaction is active on the collection (we don't
 *     want to coalesce reads across mixed tx/non-tx contexts inside one
 *     microtask)
 *
 * Tx-race detection: each queued entry remembers `_txDepth` at the time
 * `load()` was called. At flush time we compare against the current
 * depth (via the `getTxDepth` callback). If a caller enqueued OUTSIDE a
 * tx (snapshot === 0) but a tx opened before flush (current > 0), the
 * batched `find` would route through `TX_CONN` in Rust and leak the
 * non-tx read into the tx scope. We reject those entries with a clear
 * error rather than silently routing them wrong — the drain-before-begin
 * in `db.transaction` closes the common window, but a second-microtask
 * enqueue between the drain and the native `transaction(fn)` opening its
 * BEGIN remains observable.
 *
 * **P7 PR 3** — id keyspace widened from `number` to `string` (typed_id)
 * in lockstep with the Rust-side `id TEXT PRIMARY KEY` + auto-mint
 * pass (`crud::system_fields_pass::apply_system_fields_on_insert`).
 * Dedupe + map lookups now use string keys; the `Map<string, R>` value
 * type is unchanged structurally.
 */

/** A queued request waiting for the next microtask flush. */
interface QueuedLoad<R> {
  id: string;
  resolve: (row: R | null) => void;
  reject: (err: unknown) => void;
  txDepthAtEnqueue: number;
}

/**
 * Coalesces `.load(id)` calls within a microtask into a single batched
 * fetch. Construct one per Collection and reuse — it's stateless across
 * batches.
 */
export class IdLoader<R extends { id: string }> {
  private queue: QueuedLoad<R>[] = [];
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
    private flush: (ids: string[]) => Promise<Map<string, R>>,
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
  load(id: string, txDepthSnapshot: number = 0): Promise<R | null> {
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
    const liveBatch: QueuedLoad<R>[] = [];
    for (const q of batch) {
      if (q.txDepthAtEnqueue === 0 && currentTxDepth > 0) {
        q.reject(
          Object.assign(
            new Error(
              "DataLoader: batched read started outside a transaction but a " +
              "transaction opened before flush. await the get() before " +
              "db.transaction(...) to avoid this race.",
            ),
            { code: "loader_tx_race" as const },
          ),
        );
      } else {
        liveBatch.push(q);
      }
    }
    if (liveBatch.length === 0) return;

    // Dedupe ids before the underlying call — N concurrent `get("post_X")`
    // calls resolve from the same row without N copies on the wire.
    const ids: string[] = [];
    const seen = new Set<string>();
    for (const q of liveBatch) {
      if (!seen.has(q.id)) {
        seen.add(q.id);
        ids.push(q.id);
      }
    }

    try {
      const map = await this.flush(ids);
      for (const q of liveBatch) resolve(q, map);
    } catch (e) {
      for (const q of liveBatch) q.reject(e);
    }
  }
}

function resolve<R extends { id: string }>(
  q: QueuedLoad<R>,
  map: Map<string, R>,
): void {
  const row = map.get(q.id);
  q.resolve(row === undefined ? null : row);
}
