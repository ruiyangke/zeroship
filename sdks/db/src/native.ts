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
  storageKey: string,
): NativeTransactionFn | undefined {
  const holder = native as { [key: string]: unknown; transaction?: unknown };
  if (holder[storageKey] === undefined && typeof holder.transaction === "function") {
    const captured = (holder.transaction as NativeTransactionFn).bind(native);
    Object.defineProperty(native, storageKey, {
      value: captured,
      configurable: true,
      enumerable: false,
      writable: true,
    });
  }
  const transaction = holder[storageKey];
  return typeof transaction === "function"
    ? (transaction as NativeTransactionFn)
    : undefined;
}
