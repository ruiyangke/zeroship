/**
 * Host-owned DB facade installation over descriptor-bound native collections.
 * The host supplies the native handle and generated runtime descriptor.
 * Collection, transaction, relation and live-query conveniences come from the
 * public SDK implementation; authoritative schema binding and database
 * operations remain native. This adapter is bundled into zeroship-data-v8 and
 * is not an npm package entry.
 */
import { Collection } from "./collection";
import { TRANSACTION_READ } from "./crud";
import { captureNativeTransaction, type NativeDb, type NativeTransactionFn } from "../../../../packages/db/src/native";
import { Query } from "./query";
import { createLive } from "./live";
import { drainCollectionLoaders } from "./tx-state";
import { readFrom, scopeAliasedCollection } from "./read";
import type {
  AliasedCollection,
  Collections,
  LiveOptions,
  LiveQuery,
  PaginationResult,
  ReadFrom,
  SchemaInput,
  SchemaShape,
  TransactionDb,
  TxCollection,
  TxQuery,
  TransactionOptions,
} from "../../../../packages/db/src/db-types";
import {
  naming, ok, err,
} from "../../../../packages/db/src/types";
import type {
  NamingStrategy, NamedIndexSpec, Result, Row, RowId,
  RowInput, UpsertOptions, UpdateExpression, Filter,
  DistinctField, SelectInput, SortInput, SortSpec,
  WithSpec, Actor, FieldDef,
} from "../../../../packages/db/src/types";
export type { TransactionDb, TxCollection, TxQuery, TransactionOptions } from "../../../../packages/db/src/db-types";
export type { Collections, SchemaInput, SchemaShape } from "../../../../packages/db/src/db-types";

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

/** One collection in the host's decoded schema projection. */
export interface ProjectedCollection {
  fields: Record<string, FieldDef>;
  indexes?: readonly NamedIndexSpec[];
}

/**
 * The decoded schema projection the Rust host passes as `installSchema`'s
 * second argument. Rust owns descriptor validation and normalization, so the
 * adapter consumes these `FieldDef`s directly - there is no version tag and no
 * re-decode here.
 */
export interface SchemaProjection {
  collections: Record<string, ProjectedCollection>;
}

/**
 * Rehydrate the one facet whose decoded form differs from the wire. A `bytes`
 * default arrives as a JSON number array; a factory returning a fresh
 * `Uint8Array` per call keeps one insert's buffer mutation out of the next.
 */
function rehydrateProjectedFields(
  fields: Record<string, FieldDef>,
): Record<string, FieldDef> {
  const out: Record<string, FieldDef> = {};
  for (const [name, field] of Object.entries(fields)) {
    const decoded: FieldDef = { ...field };
    if (decoded.type === "bytes" && Array.isArray(decoded.default)) {
      const bytes = Uint8Array.from(decoded.default as number[]);
      decoded.default = () => bytes.slice();
    }
    if (decoded.shape) decoded.shape = rehydrateProjectedFields(decoded.shape);
    if (decoded.variants) decoded.variants = decoded.variants.map(rehydrateProjectedFields);
    out[name] = decoded;
  }
  return out;
}

// ---------------------------------------------------------------------------
// TxCollection — wraps a Collection, throws on error
// ---------------------------------------------------------------------------

async function unwrap<T>(result: Result<T>): Promise<T> {
  if (result.error) throw result.error;
  return result.data as T;
}

type TxReadHints<S> = {
  actor?: Actor;
  unmask?: (string & keyof Row<S>)[];
  unmaskReason?: string;
};

function transactionScopeExpired(): Error {
  return Object.assign(new Error("transaction scope has expired"), {
    code: "TRANSACTION_SCOPE_EXPIRED" as const,
  });
}

function requireActiveTransaction(active: () => boolean): void {
  if (!active()) throw transactionScopeExpired();
}

function createTxCollection<S>(
  collection: Collection<S>,
  active: () => boolean,
): TxCollection<S> {
  async function run<T>(work: () => Promise<Result<T>>): Promise<T> {
    requireActiveTransaction(active);
    return unwrap(await work());
  }

  async function getImpl(
    idOrFilter: RowId<S> | Filter<S>,
    opts?: {
      select?: (string & keyof Row<S>)[];
      orderBy?: SortSpec<S>;
    } & TxReadHints<S>,
  ): Promise<unknown> {
    const colAny = collection as unknown as {
      get(
        idOrFilter: RowId<S> | Filter<S>,
        opts?: {
          select?: (string & keyof Row<S>)[];
          orderBy?: SortSpec<S>;
        } & TxReadHints<S> & { [TRANSACTION_READ]?: boolean },
      ): Promise<Result<Row<S> | null>>;
    };
    return run(() => colAny.get(idOrFilter, {
      ...opts,
      [TRANSACTION_READ]: true,
    }));
  }

  const tx: TxCollection<S> = {
    as: alias => {
      requireActiveTransaction(active);
      return scopeAliasedCollection(collection.as(alias), active);
    },
    async insert(row: RowInput<S>) {
      return run(() => collection.insert(row));
    },
    async insertMany(rows: RowInput<S>[]) {
      return run(() => collection.insertMany(rows));
    },
    get: getImpl as TxCollection<S>["get"],
    async exists(filter: Filter<S>) {
      return run(() => collection.exists(filter));
    },
    find: ((filter: Filter<S> = {} as Filter<S>, opts?: { with?: WithSpec<S> } & TxReadHints<S>): TxQuery<S, Row<S>> => {
      requireActiveTransaction(active);
      const query = (collection as unknown as {
        find(f: Filter<S>, o?: { with?: WithSpec<S> } & TxReadHints<S>): Query<S, Row<S>>;
      }).find(filter, opts);
      return createTxQuery<S>(query, active);
    }) as TxCollection<S>["find"],
    async upsert(row: RowInput<S>, options: UpsertOptions<S>) {
      return run(() => collection.upsert(row, options));
    },
    async update(idOrFilter: RowId<S> | Filter<S>, patch: UpdateExpression<S>) {
      return run(() => collection.update(idOrFilter, patch));
    },
    async updateMany(filter: Filter<S>, patch: UpdateExpression<S>) {
      return run(() => collection.updateMany(filter, patch));
    },
    async delete(idOrFilter: RowId<S> | Filter<S>) {
      return run(() => collection.delete(idOrFilter));
    },
    async deleteMany(filter: Filter<S>) {
      return run(() => collection.deleteMany(filter));
    },
    async purge(idOrFilter: RowId<S> | Filter<S>) {
      return run(() => collection.purge(idOrFilter));
    },
    async purgeMany(filter: Filter<S> = {} as Filter<S>) {
      return run(() => collection.purgeMany(filter));
    },
    async restore(idOrFilter: RowId<S> | Filter<S>) {
      return run(() => collection.restore(idOrFilter));
    },
    async restoreMany(filter: Filter<S> = {} as Filter<S>) {
      return run(() => collection.restoreMany(filter));
    },
    async count(filter: Filter<S> = {} as Filter<S>) {
      return run(() => collection.count(filter));
    },
    async distinct<K extends DistinctField<S> & keyof Row<S>>(
      field: K,
      filter: Filter<S> = {} as Filter<S>,
    ): Promise<Exclude<Row<S>[K], undefined>[]> {
      return run(() => collection.distinct(field, filter));
    },
    async aggregate(pipeline: ZeroshipDbAggregateStage[]) {
      return run(() => collection.aggregate(pipeline));
    },
    async bulkUnmask(
      ...args: Parameters<Collection<S>["bulkUnmask"]>
    ) {
      return run(() => collection.bulkUnmask(...args));
    },
    async search(
      ...args: Parameters<Collection<S>["search"]>
    ) {
      return run(() => collection.search(...args));
    },
    async near(
      ...args: Parameters<Collection<S>["near"]>
    ) {
      return run(() => collection.near(...args));
    },
  };
  return tx;
}

function createTxQuery<S>(
  query: Query<S, Row<S>>,
  active: () => boolean,
): TxQuery<S, Row<S>> {
  function selectImpl(s: SelectInput<S>): unknown {
    requireActiveTransaction(active);
    (query.select as (arg: unknown) => unknown)(s);
    return wrapped;
  }
  const wrapped: TxQuery<S, Row<S>> = {
    sort(s: SortInput<S>) { requireActiveTransaction(active); query.sort(s); return wrapped; },
    limit(n: number) { requireActiveTransaction(active); query.limit(n); return wrapped; },
    skip(n: number) { requireActiveTransaction(active); query.skip(n); return wrapped; },
    select: selectImpl as TxQuery<S, Row<S>>["select"],
    after(id: RowId<S>) { requireActiveTransaction(active); query.after(id); return wrapped; },
    with: ((spec: WithSpec<S>) => {
      requireActiveTransaction(active);
      (query as unknown as { with(s: WithSpec<S>): unknown }).with(spec);
      return wrapped;
    }) as TxQuery<S, Row<S>>["with"],
    async paginate(
      opts: Parameters<Query<S, Row<S>>["paginate"]>[0],
    ): Promise<PaginationResult<Row<S>>> {
      requireActiveTransaction(active);
      return unwrap(await query.paginate(opts));
    },
    // **P9 PR 1** — Result→throw shims for the new terminals so the
    // tx-callback contract (throw, not return Result) stays uniform.
    async first(): Promise<Row<S> | null> {
      requireActiveTransaction(active);
      return unwrap(await query.first());
    },
    async unique(): Promise<Row<S>> {
      requireActiveTransaction(active);
      return unwrap(await query.unique());
    },
    async last(): Promise<Row<S> | null> {
      requireActiveTransaction(active);
      return unwrap(await query.last());
    },
    then<TResult1 = Row<S>[], TResult2 = never>(
      resolve?: ((value: Row<S>[]) => TResult1 | PromiseLike<TResult1>) | null,
      reject?: ((reason: unknown) => TResult2 | PromiseLike<TResult2>) | null,
    ): Promise<TResult1 | TResult2> {
      try {
        requireActiveTransaction(active);
      } catch (error) {
        return (Promise.reject(error) as Promise<Row<S>[]>).then(resolve, reject);
      }
      return query.then(
        (result: Result<Row<S>[]>) => {
          if (result.error) throw result.error;
          return resolve ? resolve(result.data as Row<S>[]) : (result.data as unknown as TResult1);
        },
        reject,
      ) as Promise<TResult1 | TResult2>;
    },
  };
  return wrapped;
}

// ---------------------------------------------------------------------------
// installSchema
// ---------------------------------------------------------------------------

export interface InstallSchemaOptions {
  /** Defaults to the descriptor's field spelling, without case conversion. */
  naming?: NamingStrategy;
}

const RESERVED_ENV_DB_NAMES = new Set<string>([
  "declareMaskPolicy",
  "__proto__",
  "collection",
  "constructor",
  // **P9 PR 3** — `beginTransaction` removed: the native primitive was
  // deleted entirely (transaction orchestration moved into Rust). The
  // creator-facing `transaction` (below) is now a native method on
  // `env.db`, so it stays reserved.
  "transaction",
  "live",
  "from",
]);

let _installInFlight = false;
const installedNames = new WeakMap<NativeDb, readonly string[]>();

/**
 * Framework-internal helper that installs the runtime schema descriptor.
 * Returns the typed collection map after planting it on `env.db`.
 */
export function installSchema<const T extends Record<string, SchemaInput>>(
  env: NativeDb,
  projection: SchemaProjection | undefined,
  options?: InstallSchemaOptions,
): { collections: Collections<T> } {
  if (_installInFlight) {
    throw Object.assign(
      new Error(
        "@zeroship/db: installSchema called while a previous install is in flight — " +
          "this helper must run during native plugin preparation.",
      ),
      { code: "INSTALL_IN_FLIGHT" as const },
    );
  }
  _installInFlight = true;
  try {
    return _installSchemaInner<T>(env, projection, options);
  } finally {
    _installInFlight = false;
  }
}

function _installSchemaInner<const T extends Record<string, SchemaInput>>(
  env: NativeDb,
  projection: SchemaProjection | undefined,
  options?: InstallSchemaOptions,
): { collections: Collections<T> } {
  if (env == null || typeof env !== "object") {
    throw Object.assign(
      new Error(
        "@zeroship/db: installSchema requires a native env.db handle as " +
          "the first argument — got " + (env === undefined ? "undefined" : env === null ? "null" : typeof env) + ".",
      ),
      { code: "NATIVE_DB_UNAVAILABLE" as const },
    );
  }
  const native = env;
  const namingStrategy = options?.naming ?? naming.asIs;

  const projected = projection?.collections ?? {};
  const schemas = Object.create(null) as Record<string, Record<string, FieldDef>>;
  for (const [name, collection] of Object.entries(projected)) {
    schemas[name] = rehydrateProjectedFields(collection.fields);
  }

  const collections = Object.create(null) as {
    [K in keyof T]: Collection<SchemaShape<T[K]>, K & string, T>;
  };

  // Capture the native orchestrator before installing the SDK wrapper.
  // The weak-map capture remains stable across repeated installs and cannot
  // collide with a creator table name.
  const nativeTransaction = captureNativeTransaction(
    native as unknown as object,
  ) as NativeTransactionFn | undefined;

  for (const [name, fields] of Object.entries(schemas)) {
    (collections as Record<string, Collection<unknown, string, T>>)[name] =
      new Collection(name, fields, native, {
        naming: namingStrategy,
        indexes: projected[name]?.indexes ?? [],
        schemas,
      }) as Collection<unknown, string, T>;
  }

  function liveImpl<R>(
    queryFn: () => Promise<R[]> | { then(onFulfilled: (value: unknown) => unknown, onRejected?: (reason: unknown) => unknown): unknown },
    liveOptions?: LiveOptions,
  ): LiveQuery<R> {
    if (transactionContext.getStore() === true) {
      throw Object.assign(
        new Error("@zeroship/db: db.live cannot be called inside db.transaction"),
        { code: "LIVE_IN_TRANSACTION" as const },
      );
    }
    return createLive<R>(queryFn, liveOptions);
  }

  // **P9 PR 3** — transaction orchestration moved into Rust.
  //
  // The native `env.db.transaction(callback, opts)` v8_method owns
  // begin / commit / rollback / nested-savepoint (see
  // `crates/zeroship-data-orm/src/transaction/mod.rs`). It calls
  // `callback(rawTxView)` once BEGIN/SAVEPOINT succeeds and returns a
  // promise that resolves with the callback's result on commit (callback
  // resolved) or rejects with the callback's error on rollback (callback
  // threw). BEGIN, classified session-setup, commit, savepoint, and body
  // errors are emitted by Rust and surface verbatim on the rejection
  // (`err.code`, plus `err.status` when the classification has an HTTP
  // remedy).
  //
  // This wrapper drains pending JS loaders before BEGIN, scopes the
  // creator-facing transaction handles to the callback, and maps the native
  // promise to Result. AsyncLocalStorage keeps SDK-only guards local to the
  // callback continuation while Rust owns connection routing and nesting.
  //
  // The `txCollections` (SDK collections wrapped `Result`→throw) route
  // through the tx connection automatically, since the native CRUD path
  // consults the `tx_conn` slot the orchestrator set. The native raw view
  // serves plain-JS callers; this wrapper exposes the same name lookup over
  // SDK handles with field mapping and the throwing transaction contract.
  async function transactionImpl<R>(
    fn: (tx: TransactionDb<T>) => Promise<R>,
    txOptions?: TransactionOptions,
  ): Promise<Result<R>> {
    if (nativeTransaction === undefined) {
      return err(
        Object.assign(
          new Error(
            "@zeroship/db: env.db.transaction not available — " +
              "runtime is missing the native Db.transaction(fn) orchestrator.",
          ),
          { code: "NATIVE_TRANSACTION_UNAVAILABLE" as const },
        ),
      );
    }

    const collectionList = Object.values(collections);

    // A drain failure aborts before BEGIN.
    try {
      await drainCollectionLoaders(collectionList);
    } catch (drainErr) {
      const wrapped = Object.assign(
        new Error(
          `pre-transaction drain failed: ${
            drainErr instanceof Error ? drainErr.message : String(drainErr)
          }`,
          { cause: drainErr instanceof Error ? drainErr : undefined },
        ),
        { code: "TX_DRAIN_FAILED" as const },
      );
      return err(wrapped);
    }

    try {
      // Native orchestrator: begin → callback(txCollections) →
      //    commit/rollback. Resolves with the callback's result on
      //    commit; rejects with the typed error on rollback, setup denial,
      //    begin failure, commit indeterminacy, or depth exhaustion.
      const opts = txOptions?.isolationLevel
        ? { isolationLevel: txOptions.isolationLevel }
        : undefined;
      const bodyResult = (await nativeTransaction(
        // The native view and SDK handles share the active tx connection.
        async (_rawTxView: unknown) => {
          let active = true;
          const from: ReadFrom<true> = source => readFrom(native, source, true, () => active);
          const txCollections = Object.create(null) as {
            [K in keyof T]: TxCollection<SchemaShape<T[K]>, T>;
          };
          for (const [name, col] of Object.entries(collections)) {
            (txCollections as Record<string, unknown>)[name] =
              createTxCollection(col as Collection<unknown>, () => active);
          }
          const collection = <K extends string & keyof T>(name: K) => {
            requireActiveTransaction(() => active);
            const txCollection = txCollections[name];
            if (txCollection === undefined) {
              throw Object.assign(
                new Error(`@zeroship/db: collection "${name}" is not declared`),
                { code: "COLLECTION_NOT_DECLARED" as const },
              );
            }
            return txCollection;
          };
          return transactionContext.run(true, async () => {
            try {
              return await fn({ ...txCollections, collection, from } as TransactionDb<T>);
            }
            finally { active = false; }
          });
        },
        opts,
      )) as R;
      return ok(bodyResult);
    } catch (txErr) {
      // The native rejection already carries the right code
      // (`GRANT_REVOKED`, commit/begin/savepoint codes, or a future setup
      // fence) or is the creator's own thrown error verbatim. Surface it as
      // `result.error`.
      return err(txErr instanceof Error ? txErr : new Error(String(txErr)));
    }
  }

  {
    const target = native as unknown as Record<string, unknown>;
    const newNames = Object.keys(collections);
    const newNameSet = new Set(newNames);
    const prevNames = installedNames.get(native) ?? [];
    for (const stale of prevNames) {
      if (newNameSet.has(stale)) continue;
      if (RESERVED_ENV_DB_NAMES.has(stale)) continue;
      try {
        delete target[stale];
      } catch {
        /* native v8_class may refuse the delete on a sealed prototype */
      }
    }
    for (const [name, col] of Object.entries(collections)) {
      if (
        RESERVED_ENV_DB_NAMES.has(name) ||
        (!Object.hasOwn(target, name) && name in target)
      ) {
        continue;
      }
      Object.defineProperty(target, name, {
        value: col,
        configurable: true,
        enumerable: true,
        writable: false,
      });
    }
    Object.defineProperty(target, "transaction", {
      value: transactionImpl,
      configurable: true,
      enumerable: true,
      writable: false,
    });
    Object.defineProperty(target, "from", {
      value: (source: AliasedCollection<unknown>) => readFrom(native, source),
      configurable: true,
      enumerable: true,
      writable: false,
    });
    Object.defineProperty(target, "live", {
      value: liveImpl,
      configurable: true,
      enumerable: true,
      writable: false,
    });
    installedNames.set(native, newNames);
  }

  return { collections: collections as Collections<T> };
}
