/**
 * Collection: the main entry point for CRUD operations on a named collection.
 * Each method validates inputs against the schema, maps field names to the native
 * format, calls the native driver, and maps results back to the user-facing shape.
 */
import type { IdLoader } from "./loader";
import { AliasedCollection } from "./read";
import {
  requireNativeCollection,
  type NativeCollection,
  type NativeDb,
} from "./native";
import {
  mapNativeError,
  OptimisticLockError,
  ValidationError,
} from "./errors";
import type { Query } from "./query";
import { validateCollectionIdentity, type NormalizedSchema } from "./schema";
import type {
  Actor,
  ExactWithSpec,
  Filter,
  GeoField,
  DistinctField,
  Id,
  IdValue,
  RowId,
  NamedIndexSpec,
  NamingStrategy,
  PlainObject,
  Result,
  Row,
  RowInput,
  SortSpec,
  UpsertOptions,
  UpdateExpression,
  VectorField,
  WithRelations,
  WithSpec,
} from "./types";
import { err, naming, ok } from "./types";
import {
  aggregateCollection,
  countCollection,
  deleteCollection,
  deleteManyCollection,
  distinctCollection,
  existsCollection,
  findCollection,
  getCollection,
  insertCollection,
  insertManyCollection,
  loadByIdCollection,
  purgeCollection,
  purgeManyCollection,
  restoreCollection,
  restoreManyCollection,
  upsertCollection,
  updateCollection,
  updateManyCollection,
  validateArrayPushOps,
  type CrudCollectionInternals,
} from "./collection/crud";
import {
  __zeroshipDbResetIndexWarnings,
  __zeroshipDbWarnedShapesSize,
} from "./collection/index-warnings";
import {
  bulkUnmaskCollection,
  type MaskingCollectionInternals,
} from "./collection/masking";
import {
  loadRelations,
  type RelationsCollectionInternals,
} from "./collection/relations";
import {
  nearCollection,
  searchCollection,
  type VectorGeoCollectionInternals,
} from "./collection/vector-geo";

export { validateArrayPushOps };
export { __zeroshipDbResetIndexWarnings, __zeroshipDbWarnedShapesSize };

type ReadHints<S> = {
  actor?: Actor;
  unmask?: (string & keyof Row<S>)[];
  unmaskReason?: string;
};

/** The native driver interface from @zeroship/types. */
/**
 * Converts a caught value to an Error for inclusion in a Result.
 * ValidationError instances are returned as-is (they are already well-typed).
 * All other errors are passed through mapNativeError so that, e.g., unique
 * constraint violations preserve the native string code `UNIQUE_VIOLATION`.
 */
function toResultError(e: unknown): Error {
  let out: Error;
  if (e instanceof ValidationError) out = e;
  else if (e instanceof OptimisticLockError) out = e;
  else out = mapNativeError(e);
  try {
    Object.defineProperty(out, "toJSON", {
      value: function () {
        const obj: Record<string, unknown> = {
          name: (this as Error).name,
          message: (this as Error).message,
        };
        const code = (this as { code?: unknown }).code;
        if (code !== undefined) obj.code = code;
        const errs = (this as { errors?: unknown }).errors;
        if (errs !== undefined) obj.errors = errs;
        return obj;
      },
      enumerable: false,
      configurable: true,
      writable: true,
    });
  } catch {
    /* frozen error — no-op */
  }
  return out;
}

/**
 * Represents a named collection and exposes the full CRUD + aggregate API.
 * The generic parameter `S` is the raw schema shape from which document and input
 * types are derived. `N` carries the table name as a string-literal so the
 * `Id` accessor below produces `Id<N>` rather than `Id<string>`.
 *
 * `AllSchemas` is the parent db's full schema map — threaded in by
 * `installSchema` so a `find({...}, { with: { userId: true } })` can
 * resolve the joined field's type to the target collection's `Row<...>`
 * rather than the v1 fallback of `PlainObject`. Standalone `model()`
 * callers inherit the safe default and degrade to `PlainObject` per
 * relation.
 *
 * Use `model()` in tests/shared packages, or access app collections through the
 * generated `env.db.<name>` surface — do not construct directly.
 */
export class Collection<
  S = PlainObject,
  N extends string = string,
  AllSchemas extends Record<string, unknown> = Record<string, unknown>,
> {
  private _name: string;
  private _schema: NormalizedSchema;
  private _native: NativeDb;
  /** Lazily resolved Collection v8_class instance — see `_nativeCollection()`. */
  private _nativeCol: NativeCollection | null;
  private _knownFields: Set<string>;
  private _toColumn: (field: string) => string;
  private _toField: (column: string) => string;
  /**
   * Named multi-column indexes declared via `schema(...).index(name, fields)`.
   * Field names are already mapped to column names so the runtime warning
   * compares them against filters that have also been column-mapped.
   */
  private _indexes: readonly NamedIndexSpec[];
  /** Per-collection DataLoader, lazily constructed on first batchable `get(id)`. */
  private _idLoader: IdLoader<Row<S>, IdValue> | null;
  /**
   * Sibling-collection lookup, planted by `installSchema` so `with: { fk: true }`
   * can resolve `fieldDef.refTarget` → the target `Collection` to fire one
   * batched `find({id: {$in: ids}})` against. `model()` callers without a
   * parent db leave this null; `with` then errors at call time with a
   * clear message instead of silently degrading to N+1.
   */
  private _resolveCollection:
    | ((name: string) => Collection<unknown> | undefined)
    | null;
  /**
   * Active-transaction depth. `db.transaction()` wraps `tx.x.*` calls
   * with an increment/decrement so the loader is bypassed while a tx is
   * live on this collection — see `tx-state.ts`. Mixing a
   * batched read with `TX_CONN`-routed reads in the same microtask
   * would otherwise blur the connection-routing boundary.
   */
  private _txDepth: number;

  declare readonly Id: Id<N, RowId<S>>;
  declare readonly RowInput: RowInput<S>;

  as<const A extends string>(alias: A): AliasedCollection<Row<S>, A> {
    return new AliasedCollection(this._native, this._name, alias, Object.keys(this._schema), this._toColumn, this._toField);
  }

  constructor(
    name: string,
    schema: NormalizedSchema,
    native: NativeDb,
    options?: {
      naming?: NamingStrategy;
      indexes?: readonly NamedIndexSpec[];
    },
  ) {
    validateCollectionIdentity(schema);
    this._name = name;
    this._schema = schema;
    this._native = native;
    this._nativeCol = null;
    this._idLoader = null;
    this._txDepth = 0;
    this._resolveCollection = null;

    const strategy = options?.naming ?? naming.asIs;
    const fieldToCol: Record<string, string> = {};
    const colToField: Record<string, string> = {};
    for (const field of Object.keys(schema)) {
      const col = strategy.toColumn(field);
      fieldToCol[field] = col;
      colToField[col] = field;
    }
    this._knownFields = new Set(Object.keys(fieldToCol));
    this._toColumn = (field) => fieldToCol[field] ?? field;
    this._toField = (column) => colToField[column] ?? column;

    this._indexes = (options?.indexes ?? []).map((idx) => ({
      name: idx.name,
      fields: [...idx.fields],
      ...(idx.unique ? { unique: true } : {}),
    }));
  }

  private _crud(): CrudCollectionInternals<S, N, AllSchemas> {
    return this as unknown as CrudCollectionInternals<S, N, AllSchemas>;
  }

  private _relations(): RelationsCollectionInternals {
    return this as unknown as RelationsCollectionInternals;
  }

  private _masking(): MaskingCollectionInternals<S> {
    return this as unknown as MaskingCollectionInternals<S>;
  }

  private _vectorGeo(): VectorGeoCollectionInternals<S> {
    return this as unknown as VectorGeoCollectionInternals<S>;
  }

  /**
   * Resolve the Collection v8_class instance for this collection name.
   * Cached on first call so subsequent CRUD ops are a single property
   * read. The native runtime exposes `env.db.collection(name)` as a
   * Db v8_method that returns a typed Collection wrapper; calling it
   * twice with the same `name` returns the same JS object (identity is
   * cached on the Db wrapper).
   */
  private _nativeCollection(): NativeCollection {
    if (this._nativeCol) return this._nativeCol;
    this._nativeCol = requireNativeCollection(this._native, this._name, {
      code: "NATIVE_COLLECTION_UNAVAILABLE",
      message: "@zeroship/db: env.db.collection(name) is unavailable",
    });
    return this._nativeCol;
  }

  /** @internal — planted by `installSchema` so `with` can resolve siblings. */
  _setResolveCollection(
    fn: (name: string) => Collection<unknown> | undefined,
  ): void {
    this._resolveCollection = fn;
  }

  async _loadRelations(rows: PlainObject[], withSpec: WithSpec): Promise<void> {
    return loadRelations(this._relations(), rows, withSpec);
  }

  /** Wraps an operation in try/catch and maps it to Result. */
  private async _run<T>(fn: () => Promise<T>): Promise<Result<T>> {
    try {
      return ok(await fn());
    } catch (e) {
      return err(toResultError(e));
    }
  }

  private _toResultError(e: unknown): Error {
    return toResultError(e);
  }

  private async _loadById(
    id: IdValue,
    txDepthAtCall: number,
  ): Promise<Row<S> | null> {
    return loadByIdCollection(this._crud(), id, txDepthAtCall);
  }

  async insert(row: RowInput<S>): Promise<Result<Row<S>>> {
    return insertCollection(this._crud(), row);
  }

  async insertMany(rows: RowInput<S>[]): Promise<Result<Row<S>[]>> {
    return insertManyCollection(this._crud(), rows);
  }

  async get<K extends string & keyof Row<S>>(
    idOrFilter: RowId<S> | Filter<S>,
    opts: { select: K[]; orderBy?: SortSpec<S> } & ReadHints<S>,
  ): Promise<Result<Pick<Row<S>, K> | null>>;
  async get<const W extends WithSpec<S>>(
    idOrFilter: RowId<S> | Filter<S>,
    opts: { with: ExactWithSpec<S, W>; orderBy?: SortSpec<S> } & ReadHints<S>,
  ): Promise<Result<(Omit<Row<S>, keyof W> & WithRelations<S, W, AllSchemas>) | null>>;
  async get(
    idOrFilter: RowId<S> | Filter<S>,
    opts?: { orderBy?: SortSpec<S> } & ReadHints<S>,
  ): Promise<Result<Row<S> | null>>;
  async get(
    idOrFilter: RowId<S> | Filter<S>,
    opts: {
      actor?: Actor;
      select?: (string & keyof Row<S>)[];
      orderBy?: SortSpec<S>;
      unmask?: (string & keyof Row<S>)[];
      unmaskReason?: string;
      with?: WithSpec;
    } = {},
    // The implementation signature's return type must be compatible with
    // EVERY overload above, including the `with` overload whose joined key
    // OMITS the raw FK field from `Row<S>` before intersecting the joined
    // shape back in (see WithRelations doc comment in types.ts). `any` is
    // the standard TS idiom here — callers never see this signature, only
    // the precise overloads above, which stay fully checked.
  ): Promise<Result<any>> {
    return getCollection(this._crud(), idOrFilter, opts);
  }

  async exists(filter: Filter<S> = {} as Filter<S>): Promise<Result<boolean>> {
    return existsCollection(this._crud(), filter);
  }

  find<const W extends WithSpec<S>>(
    filter: Filter<S>,
    opts: { with: ExactWithSpec<S, W> } & ReadHints<S>,
  ): Query<S, Omit<Row<S>, keyof W> & WithRelations<S, W, AllSchemas>, AllSchemas>;
  find(filter?: Filter<S>): Query<S, Row<S>, AllSchemas>;
  find(
    filter: Filter<S> = {} as Filter<S>,
    opts?: { with?: WithSpec } & ReadHints<S>,
    // Same `any` rationale as `get` above.
  ): Query<S, any, AllSchemas> {
    return findCollection(this._crud(), filter, opts);
  }

  async upsert(
    row: RowInput<S>,
    options: UpsertOptions<S>,
  ): Promise<Result<Row<S>>> {
    return upsertCollection(this._crud(), row, options);
  }

  async update(
    idOrFilter: RowId<S> | Filter<S>,
    patch: UpdateExpression<S>,
  ): Promise<Result<Row<S> | null>> {
    return updateCollection(this._crud(), idOrFilter, patch);
  }

  async updateMany(
    filter: Filter<S> = {} as Filter<S>,
    update: UpdateExpression<S>,
  ): Promise<Result<{ count: number }>> {
    return updateManyCollection(this._crud(), filter, update);
  }

  async delete(
    idOrFilter: RowId<S> | Filter<S>,
  ): Promise<Result<Row<S> | null>> {
    return deleteCollection(this._crud(), idOrFilter);
  }

  async deleteMany(
    filter: Filter<S> = {} as Filter<S>,
  ): Promise<Result<{ deletedCount: number }>> {
    return deleteManyCollection(this._crud(), filter);
  }

  async purge(
    idOrFilter: RowId<S> | Filter<S>,
  ): Promise<Result<Row<S> | null>> {
    return purgeCollection(this._crud(), idOrFilter);
  }

  async purgeMany(
    filter: Filter<S> = {} as Filter<S>,
  ): Promise<Result<{ purgedCount: number }>> {
    return purgeManyCollection(this._crud(), filter);
  }

  async restore(
    idOrFilter: RowId<S> | Filter<S>,
  ): Promise<Result<Row<S> | null>> {
    return restoreCollection(this._crud(), idOrFilter);
  }

  async restoreMany(
    filter: Filter<S> = {} as Filter<S>,
  ): Promise<Result<{ restoredCount: number }>> {
    return restoreManyCollection(this._crud(), filter);
  }

  async count(filter: Filter<S> = {} as Filter<S>): Promise<Result<number>> {
    return countCollection(this._crud(), filter);
  }

  async distinct<K extends DistinctField<S> & keyof Row<S>>(
    field: K,
    filter: Filter<S> = {} as Filter<S>,
  ): Promise<Result<Exclude<Row<S>[K], undefined>[]>> {
    return distinctCollection(this._crud(), field, filter);
  }

  async aggregate(
    pipeline: ZeroshipDbAggregateStage[],
  ): Promise<Result<PlainObject[]>> {
    return aggregateCollection(this._crud(), pipeline);
  }

  async bulkUnmask(
    items: ReadonlyArray<{
      id: RowId<S>;
      columns: readonly (string & keyof Row<S>)[];
    }>,
    opts: { actor: Actor; reason?: string },
  ): Promise<Result<Map<RowId<S>, Record<string, unknown>>>> {
    return bulkUnmaskCollection(this._masking(), items, opts);
  }

  async search(
    args: {
      vector: number[];
      k?: number;
      metric?: import("./types").VectorMetric;
      column?: VectorField<S>;
      filter?: Filter<S>;
    },
  ): Promise<Result<(Row<S> & { _distance?: number })[]>> {
    return searchCollection(this._vectorGeo(), args);
  }

  async near(args: {
    field: GeoField<S>;
    point: { lat: number; lng: number };
    radius: number;
    filter?: Filter<S>;
    limit?: number;
  }): Promise<Result<(Row<S> & { _distance_m: number })[]>> {
    return nearCollection(this._vectorGeo(), args);
  }
}
