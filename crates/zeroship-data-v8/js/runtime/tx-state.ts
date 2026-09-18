import type { IdLoader } from "./loader";
import type { IdValue } from "../../../../packages/db/src/types";

type LoaderRow = { id: IdValue };

/**
 * JS loader state carried by each Collection.
 */
export interface TransactionStateCarrier {
  _idLoader: IdLoader<LoaderRow> | null;
}

function isDrainableIdLoader(value: unknown): value is Pick<IdLoader<LoaderRow>, "_drain"> {
  return value !== null && typeof value === "object" && typeof (value as { _drain?: unknown })._drain === "function";
}

function requireTransactionStateCarrier(value: unknown): TransactionStateCarrier {
  if (value === null || typeof value !== "object") {
    throw new Error("@zeroship/db: expected a Collection transaction-state carrier.");
  }
  const carrier = value as {
    _idLoader?: unknown;
  };
  if (
    carrier._idLoader !== null &&
    carrier._idLoader !== undefined &&
    !isDrainableIdLoader(carrier._idLoader)
  ) {
    throw new Error("@zeroship/db: transaction-state carrier has a non-drainable _idLoader.");
  }
  return carrier as TransactionStateCarrier;
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
