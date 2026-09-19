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
} from "../../../../packages/db/src/native";
import {
  OptimisticLockError,
  ValidationError,
} from "../../../../packages/db/src/errors";
import type { Query } from "./query";
import { validateCollectionIdentity, type NormalizedSchema } from "../../../../packages/db/src/schema";
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
} from "../../../../packages/db/src/types";
import { err, naming, ok } from "../../../../packages/db/src/types";
import type { Collection as CollectionContract } from "../../../../packages/db/src/db-types";
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
} from "./crud";
import {
  __zeroshipDbResetIndexWarnings,
  __zeroshipDbWarnedShapesSize,
} from "./index-warnings";
import {
  bulkUnmaskCollection,
  type MaskingCollectionInternals,
} from "./masking";
import { createReadResultMapper, type ReadResultMapper } from "./read-mapping";
import {
  nearCollection,
  searchCollection,
  type VectorGeoCollectionInternals,
} from "./vector-geo";
import { mapNativeError } from "./errors";

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
 * `AllSchemas` is the parent db's full schema map — threaded in by the host
 * facade so a `find({...}, { with: { user: true } })` can
 * resolve the relation's type to the target collection's `Row<...>`
 * instead of `PlainObject`.
 *
 * Access app collections through the generated `env.db.<name>` surface.
 */
export class Collection<
  S = PlainObject,
  N extends string = string,
  AllSchemas extends Record<string, unknown> = Record<string, unknown>,
> implements CollectionContract<S, N, AllSchemas> {
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
  private _mapReadResult: ReadResultMapper;
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
      schemas?: Readonly<Record<string, NormalizedSchema>>;
    },
  ) {
    validateCollectionIdentity(schema);
    this._name = name;
    this._schema = schema;
    this._native = native;
    this._nativeCol = null;
    this._idLoader = null;

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
    this._mapReadResult = createReadResultMapper(schema, options?.schemas ?? {}, strategy, this._toField);

    this._indexes = (options?.indexes ?? []).map((idx) => ({
      name: idx.name,
      fields: [...idx.fields],
      ...(idx.unique ? { unique: true } : {}),
    }));
  }

  private _crud(): CrudCollectionInternals<S, N, AllSchemas> {
    return this as unknown as CrudCollectionInternals<S, N, AllSchemas>;
  }

  private _masking(): MaskingCollectionInternals<S> {
    return this as unknown as MaskingCollectionInternals<S>;
  }

  private _vectorGeo(): VectorGeoCollectionInternals<S> {
    return this as unknown as VectorGeoCollectionInternals<S>;
  }

  /**
   * Resolve the Collection v8_class instance for this collection name.
   * Cached on first call so subsequent CRUD ops are a single property read.
   * The native runtime caches the V8 wrapper by collection name.
   */
  private _nativeCollection(): NativeCollection {
    if (this._nativeCol) return this._nativeCol;
    this._nativeCol = requireNativeCollection(this._native, this._name, {
      code: "NATIVE_COLLECTION_UNAVAILABLE",
      message: "@zeroship/db: env.db.collection(name) is unavailable",
    });
    return this._nativeCol;
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

  private async _loadById(id: IdValue): Promise<Row<S> | null> {
    return loadByIdCollection(this._crud(), id);
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
  async get<const W extends WithSpec<S>, K extends string & keyof Row<S> = string & keyof Row<S>>(
    idOrFilter: RowId<S> | Filter<S>,
    opts: { with: ExactWithSpec<S, W>; select?: K[]; orderBy?: SortSpec<S> } & ReadHints<S>,
  ): Promise<Result<(Pick<Row<S>, K> & WithRelations<S, W, AllSchemas>) | null>>;
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
  ): Promise<Result<any>> {
    return getCollection(this._crud(), idOrFilter, opts);
  }

  async exists(filter: Filter<S> = {} as Filter<S>): Promise<Result<boolean>> {
    return existsCollection(this._crud(), filter);
  }

  find<const W extends WithSpec<S>>(
    filter: Filter<S>,
    opts: { with: ExactWithSpec<S, W> } & ReadHints<S>,
  ): Query<S, Row<S> & WithRelations<S, W, AllSchemas>, AllSchemas>;
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
      metric?: import("../../../../packages/db/src/types").VectorMetric;
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
