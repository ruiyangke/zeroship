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
 *   - numeric id only (not a Filter object)
 *   - no `opts.select` (would force per-projection bucketing)
 *   - no `opts.orderBy` (irrelevant for id reads; falls through to be safe)
 *   - skipped while a transaction is active on the collection (we don't
 *     want to coalesce reads across mixed tx/non-tx contexts inside one
 *     microtask)
 */

/** A queued request waiting for the next microtask flush. */
interface QueuedLoad<R> {
  id: number;
  resolve: (row: R | null) => void;
  reject: (err: unknown) => void;
}

/**
 * Coalesces `.load(id)` calls within a microtask into a single batched
 * fetch. Construct one per Collection and reuse — it's stateless across
 * batches.
 */
export class IdLoader<R extends { id: number }> {
  private queue: QueuedLoad<R>[] = [];
  private scheduled = false;

  /**
   * @param flush  Called with the deduped id list when the microtask
   *               fires. Must resolve a `Map<id, row>`; ids without a
   *               matching row are reported back as `null`.
   */
  constructor(private flush: (ids: number[]) => Promise<Map<number, R>>) {}

  /**
   * Queue a single id-fetch. The returned promise resolves on the next
   * microtask after `flush` settles.
   */
  load(id: number): Promise<R | null> {
    return new Promise<R | null>((resolve, reject) => {
      this.queue.push({ id, resolve, reject });
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

    // Dedupe ids before the underlying call — N concurrent `get(7)` calls
    // resolve from the same row without N copies on the wire.
    const ids: number[] = [];
    const seen = new Set<number>();
    for (const q of batch) {
      if (!seen.has(q.id)) {
        seen.add(q.id);
        ids.push(q.id);
      }
    }

    try {
      const map = await this.flush(ids);
      for (const q of batch) resolve(q, map);
    } catch (e) {
      for (const q of batch) q.reject(e);
    }
  }
}

function resolve<R extends { id: number }>(
  q: QueuedLoad<R>,
  map: Map<number, R>,
): void {
  const row = map.get(q.id);
  q.resolve(row === undefined ? null : row);
}
