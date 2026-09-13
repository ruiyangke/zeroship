type RequiredSurfaceError = {
  code: string;
  message: string;
};

export interface NativeSubscriptionLike<TEvent = unknown> {
  ready(): Promise<void>;
  next(): Promise<TEvent | null>;
  close(): void;
}

export interface NativeCollection extends ZeroshipCollection {
  near: (args: {
    field: string;
    point: { lat: number; lng: number };
    radius: number;
    filter?: ZeroshipDbFilter;
    limit?: number;
  }) => Promise<Record<string, unknown>[]>;
}

export type NativeTransactionFn = (
  callback: (rawTxView: unknown) => unknown,
  opts?: { isolationLevel?: string },
) => Promise<unknown>;

export interface NativeDb extends Omit<ZeroshipDb, "collection" | "transaction"> {
  collection(name: string): NativeCollection;
  transaction: NativeTransactionFn;
}

const nativeTransactions = new WeakMap<object, NativeTransactionFn>();

function throwRequiredSurfaceError(error: RequiredSurfaceError): never {
  throw Object.assign(new Error(error.message), { code: error.code });
}

export function requireCollectionResolver(
  db: unknown,
  error: RequiredSurfaceError,
): (name: string) => NativeCollection {
  const collection = (db as { collection?: unknown } | null | undefined)?.collection;
  if (typeof collection !== "function") {
    throwRequiredSurfaceError(error);
  }
  return (name: string) => (collection as (name: string) => NativeCollection).call(db, name);
}

export function requireNativeCollection(
  db: unknown,
  name: string,
  error: RequiredSurfaceError,
): NativeCollection {
  return requireCollectionResolver(db, error)(name);
}

export function captureNativeTransaction(
  native: object,
): NativeTransactionFn | undefined {
  const captured = nativeTransactions.get(native);
  if (captured !== undefined) return captured;
  const transaction = (native as { transaction?: unknown }).transaction;
  if (typeof transaction !== "function") return undefined;
  const bound = (transaction as NativeTransactionFn).bind(native);
  nativeTransactions.set(native, bound);
  return bound;
}
