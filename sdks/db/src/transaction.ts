/**
 * Transaction wrapper for @zeroship/db.
 *
 * Usage:
 *   import { transaction } from "@zeroship/db"
 *
 *   const { data, error } = await transaction(async () => {
 *     const { data: emp } = await employees.create({ name: "Alice" });
 *     await departments.updateOne({ id: 1 }, { headcount: { $inc: 1 } });
 *     return emp;
 *   });
 *   // Auto-commits on success, auto-rollbacks on error.
 *
 * All Collection operations within the callback use the same
 * database connection. If the callback throws or returns an error,
 * the transaction is rolled back.
 */

import { ok, err, type Result } from "./types.js";

/** Get the native zeroship.db driver */
function getNativeDb(): any {
  if (typeof globalThis !== "undefined" && (globalThis as any).zeroship?.db) {
    return (globalThis as any).zeroship.db;
  }
  throw new Error("@zeroship/db: native zeroship.db.* not available");
}

/**
 * Execute a function within a database transaction.
 *
 * All `@zeroship/db` operations inside `fn` use the same connection.
 * On success: auto-commits and returns `{ data, error: null }`.
 * On error: auto-rollbacks and returns `{ data: null, error }`.
 *
 * Nested transactions are not supported — calling `transaction()` inside
 * another `transaction()` will throw.
 */
export async function transaction<T>(fn: () => Promise<T>): Promise<Result<T>> {
  const native = getNativeDb();

  // BEGIN
  await native.beginTransaction();

  try {
    const result = await fn();
    // COMMIT
    await native.commitTransaction();
    return ok(result);
  } catch (e) {
    // ROLLBACK
    try {
      await native.rollbackTransaction();
    } catch {
      // Ignore rollback errors — connection will be cleaned up
    }
    return err(e instanceof Error ? e : new Error(String(e)));
  }
}
