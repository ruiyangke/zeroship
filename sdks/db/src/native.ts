import { env } from "zeroship";

type RequiredSurfaceError = {
  code: string;
  message: string;
};

export interface NativeSubscriptionLike<TEvent = unknown> {
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

export function nativeDbFromEnv(): unknown {
  return (env as { db?: unknown } | undefined)?.db;
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

export function requireNativeCapability<TFunc extends (...args: any[]) => any>(
  capability: TFunc | undefined,
  error: RequiredSurfaceError,
): TFunc {
  if (typeof capability !== "function") {
    throwRequiredSurfaceError(error);
  }
  return capability;
}

export function requireBoundNativeCapability<
  TObject extends object,
  TKey extends keyof TObject,
>(
  owner: TObject,
  key: TKey,
  error: RequiredSurfaceError,
): TObject[TKey] extends (...args: any[]) => any ? TObject[TKey] : never {
  const capability = owner[key];
  if (typeof capability !== "function") {
    throwRequiredSurfaceError(error);
  }
  return capability.bind(owner) as TObject[TKey] extends (...args: any[]) => any
    ? TObject[TKey]
    : never;
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
