import type { IdLoader } from "./loader.js";

type LoaderRow = { id: string };

/**
 * Internal JS-only transaction bookkeeping carried by each Collection.
 *
 * The native runtime owns BEGIN / SAVEPOINT / COMMIT / ROLLBACK and the
 * actual tx connection routing. The SDK still needs two bits of JS state:
 * `_txDepth` for loader/live guards, and `_idLoader` so bootstrap can
 * flush pending batches before opening a transaction.
 */
export interface TransactionStateCarrier {
  _txDepth: number;
  _idLoader: IdLoader<LoaderRow> | null;
}

function isDrainableIdLoader(value: unknown): value is Pick<IdLoader<LoaderRow>, "_drain"> {
  return value !== null && typeof value === "object" && typeof (value as { _drain?: unknown })._drain === "function";
}

function requireTransactionStateCarrier(value: unknown): TransactionStateCarrier {
  if (value === null || typeof value !== "object") {
    throw new Error("@zeroship/db/internal: expected a Collection transaction-state carrier.");
  }
  const carrier = value as {
    _txDepth?: unknown;
    _idLoader?: unknown;
  };
  if (typeof carrier._txDepth !== "number") {
    throw new Error("@zeroship/db/internal: transaction-state carrier is missing numeric _txDepth.");
  }
  if (
    carrier._idLoader !== null &&
    carrier._idLoader !== undefined &&
    !isDrainableIdLoader(carrier._idLoader)
  ) {
    throw new Error("@zeroship/db/internal: transaction-state carrier has a non-drainable _idLoader.");
  }
  return carrier as TransactionStateCarrier;
}

export function readTransactionDepth(value: unknown): number {
  if (value === null || typeof value !== "object") return 0;
  const depth = (value as { _txDepth?: unknown })._txDepth;
  return typeof depth === "number" ? depth : 0;
}

export function anyCollectionInTransaction(db: Record<string, unknown>): boolean {
  for (const value of Object.values(db)) {
    if (readTransactionDepth(value) > 0) return true;
  }
  return false;
}

export async function drainCollectionLoaders(collections: Iterable<unknown>): Promise<void> {
  const drains: Promise<void>[] = [];
  for (const collection of collections) {
    const carrier = requireTransactionStateCarrier(collection);
    const drain = carrier._idLoader?._drain();
    if (drain !== undefined) drains.push(drain);
  }
  await Promise.all(drains);
}

export function enterTransactionScope(
  collections: Iterable<unknown>,
): TransactionStateCarrier[] {
  const carriers: TransactionStateCarrier[] = [];
  for (const collection of collections) {
    const carrier = requireTransactionStateCarrier(collection);
    carrier._txDepth += 1;
    carriers.push(carrier);
  }
  return carriers;
}

export function exitTransactionScope(collections: Iterable<TransactionStateCarrier>): void {
  for (const collection of collections) {
    collection._txDepth -= 1;
  }
}
