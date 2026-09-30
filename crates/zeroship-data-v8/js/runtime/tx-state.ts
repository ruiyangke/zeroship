import type { IdLoader } from "./loader";
import type { IdValue } from "../../../../packages/db/src/types";

type LoaderRow = { id: IdValue };

type AsyncLocalStorageLike<T> = {
  getStore(): T | undefined;
  run<R>(store: T, callback: () => R): R;
};
type AsyncLocalStorageConstructor = new <T>() => AsyncLocalStorageLike<T>;
const asyncHooksSpecifier = "node:" + "async_hooks";
const { AsyncLocalStorage } = await import(asyncHooksSpecifier) as {
  AsyncLocalStorage: AsyncLocalStorageConstructor;
};
const transactionContext = new AsyncLocalStorage<boolean>();

/**
 * Run `callback` as a transaction callback. Code it runs, and every
 * continuation of that code, reports {@link inTransactionCallback}.
 */
export function runInTransactionCallback<R>(callback: () => R): R {
  return transactionContext.run(true, callback);
}

/**
 * Whether the calling code runs inside a transaction callback. Its database
 * calls belong to that transaction, which admits one call at a time, so the
 * SDK issues each of them when it is made rather than deferring any of them
 * into a batch other code shares.
 */
export function inTransactionCallback(): boolean {
  return transactionContext.getStore() === true;
}

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
