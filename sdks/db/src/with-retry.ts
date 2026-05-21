/**
 * `withRetry` — a small helper for the optimistic-concurrency retry
 * loop that today lives at every CAS-update call site.
 *
 * The default predicate matches `OptimisticLockError` (its `.code` is
 * `"optimistic_lock_failure"`). Users who want to retry on other
 * coded errors (e.g. Postgres `serialization_failure`) can compose
 * via `isOptimisticLockError`:
 *
 * ```ts
 * await withRetry(() => db.x.update(...), {
 *   on: (e) => isOptimisticLockError(e) || (e as { code?: string }).code === "serialization_failure",
 * });
 * ```
 *
 * The helper does NOT swallow errors — the final attempt's throw bubbles up.
 */

/**
 * Default retry predicate. Returns `true` iff `e.code === "optimistic_lock_failure"`,
 * which is the `code` stamped on `OptimisticLockError` and on plain
 * `Error`s the runtime mints with the same string. Exported so callers
 * can OR it with their own predicates instead of redefining the match.
 */
export function isOptimisticLockError(e: unknown): boolean {
  return (
    e instanceof Error &&
    (e as { code?: unknown }).code === "optimistic_lock_failure"
  );
}

/** Options accepted by `withRetry`. All fields are optional. */
export interface WithRetryOptions {
  /** Maximum attempts including the first call. Default: 3. */
  max?: number;
  /** Predicate run on each thrown error to decide whether to retry. Default: {@link isOptimisticLockError}. */
  on?: (e: Error) => boolean;
  /** Milliseconds to wait before the next attempt. `attempt` is 1-indexed
   *  (1 after the first failure, 2 after the second, ...). Default: () => 0. */
  backoff?: (attempt: number) => number;
}

/**
 * Run `fn`, retrying on errors matched by `opts.on`. Returns `fn`'s
 * resolved value on success; rethrows the last error if every attempt
 * fails (or if a thrown error doesn't match the predicate).
 *
 * ```ts
 * const row = await withRetry(async () => {
 *   const { data: cur } = await db.products.get(id);
 *   if (!cur) throw new Error("not found");
 *   const { data, error } = await db.products.update(
 *     { id, version: cur.version },
 *     { stock: { $dec: 1 } },
 *   );
 *   if (error) throw error;
 *   return data;
 * });
 * ```
 */
export async function withRetry<T>(
  fn: () => Promise<T>,
  opts?: WithRetryOptions,
): Promise<T> {
  const max = opts?.max ?? 3;
  if (!Number.isInteger(max) || max <= 0) {
    throw Object.assign(
      new TypeError("withRetry: opts.max must be a positive integer"),
      { code: "with_retry_invalid_max" as const },
    );
  }
  const on = opts?.on ?? isOptimisticLockError;
  const backoff = opts?.backoff ?? (() => 0);

  let attempt = 0;
  // We loop up to `max` total attempts. After each failure we ask the
  // predicate whether to retry, and (if so) wait `backoff(attempt)` ms.
  // The final attempt's throw bubbles up unchanged so callers see the
  // exact Error their fn raised.
  // eslint-disable-next-line no-constant-condition
  while (true) {
    attempt += 1;
    try {
      return await fn();
    } catch (e) {
      const err = e instanceof Error ? e : new Error(String(e));
      const exhausted = attempt >= max;
      if (exhausted || !on(err)) throw e;
      const delay = backoff(attempt);
      if (delay > 0) {
        await new Promise((resolve) => setTimeout(resolve, delay));
      }
    }
  }
}
