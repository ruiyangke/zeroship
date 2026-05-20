/**
 * Collection: the main entry point for CRUD operations on a named collection.
 * Each method validates inputs against the schema, maps field names to the native
 * format, calls the native driver, and maps results back to the user-facing shape.
 */
import { NormalizedSchema } from "./schema.js";
import { validateDoc, checkPartial } from "./validate.js";
import { mapNativeError, ValidationError, OptimisticLockError } from "./errors.js";
import {
  mapResultDoc,
  mapDocOutbound,
  mapFilterOutbound,
  mapUpdateOutbound,
  translateAggregatePipeline,
} from "./utils.js";
import { Query } from "./query.js";
import { IdLoader } from "./loader.js";
import { trackCollectionAccess } from "./live.js";
import { PlainObject, Result, Row, RowInput, UpdateExpression, Filter, type Id, type NamingStrategy, type NamedIndexSpec, naming, ok, err } from "./types.js";

/** The native driver interface from @zeroship/types. */
export type NativeDb = ZeroshipDb;

/** The native Collection wrapper from @zeroship/types. */
export type NativeCollection = ZeroshipCollection;

/**
 * Converts a caught value to an Error for inclusion in a Result.
 * ValidationError instances are returned as-is (they are already well-typed).
 * All other errors are passed through mapNativeError so that, e.g., unique
 * constraint violations receive code 11000.
 */
function toResultError(e: unknown): Error {
  let out: Error;
  if (e instanceof ValidationError) out = e;
  else if (e instanceof OptimisticLockError) out = e;
  // Pass the whole value through mapNativeError — when the native side
  // already threw an Error with a `.code` (e.g. "migration_already_running"),
  // mapNativeError returns it untouched so the structured code reaches the
  // caller via `result.error.code`.
  else out = mapNativeError(e);
  // Errors serialize to `{}` by default (message/name are
  // non-enumerable). Attach `toJSON` so the RPC wire
  // (`JSON.stringify({ data, error })`) preserves message + code
  // instead of dropping them.
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
 * Extracts the plain field map from an update argument for validation.
 * Handles both `{ $set: { field: val } }` and bare `{ field: val }` styles.
 * `$push`, `$addToSet`, `$inc`, `$dec`, `$mul`, and other operators are excluded.
 */
function extractUpdateFields(update: PlainObject): PlainObject {
  const fields: PlainObject = {};
  for (const [key, val] of Object.entries(update)) {
    if (key === "$set" && typeof val === "object" && val !== null) {
      for (const k of Object.keys(val as PlainObject)) {
        if (k === "__proto__" || k === "constructor" || k === "prototype") continue;
        fields[k] = (val as PlainObject)[k];
      }
    } else if (!key.startsWith("$")) {
      // Skip per-field operator objects like { $inc: 1 } — they are not plain values
      if (typeof val === "object" && val !== null && !Array.isArray(val) &&
          Object.keys(val as PlainObject).every(k => k.startsWith("$"))) continue;
      fields[key] = val;
    }
  }
  return fields;
}

/**
 * Validates $push / $addToSet values against the schema's array item type.
 * Throws ValidationError if any pushed value does not match the declared items type.
 * Numeric operators ($inc, $dec, $mul) are skipped — they are inherently numeric.
 */
function validateArrayPushOps(
  update: PlainObject,
  schema: NormalizedSchema
): void {
  for (const op of ["$push", "$addToSet"] as const) {
    const opVal = update[op];
    if (opVal === null || typeof opVal !== "object") continue;

    for (const [field, val] of Object.entries(opVal as PlainObject)) {
      const def = schema[field];
      if (!def || def.type !== "array" || !def.items) continue;
      const itemType = def.items;

      let valid = true;
      if (itemType === "string") valid = typeof val === "string";
      else if (itemType === "number") valid = typeof val === "number";
      else if (itemType === "boolean") valid = typeof val === "boolean";
      else if (itemType === "date") valid = val instanceof Date || typeof val === "string";

      if (!valid) {
        throw new ValidationError({
          [field]: {
            path: field,
            message: `${op} value for ${field} must be a ${itemType}`,
          },
        });
      }
    }
  }
}

/**
 * D1 — set of `${collection}:${sortedFilterKeys}` shapes already warned
 * about. Module-scope so a single warning fires per shape across all
 * Collection instances in the same isolate. Reset between tests by
 * accessing `__zeroshipDbWarnedShapesForTest()`.
 */
const _warnedShapes: Set<string> = new Set();

/** @internal — test-only reset hook. Not part of the public API. */
export function __zeroshipDbResetIndexWarnings(): void {
  _warnedShapes.clear();
}

/**
 * D1 — emit a one-time `console.warn` if `filter` would do a sequential
 * scan because no declared index covers its keys. Coverage rule: an
 * index `{name, fields: [f1, f2, ...]}` covers the filter when the
 * filter's key set is a non-empty prefix of `fields` (Postgres can use
 * a multi-column B-tree for any leftmost-prefix subset). Single-field
 * `.unique()` / `.index()` markers are still recognised — they desugar
 * to a single-column index. Only fires when `process.env.NODE_ENV !==
 * "production"`. Deduplicates by `${collection}:${sortedKeys}`.
 */
function _maybeWarnUnindexedFilter(
  collection: string,
  schema: NormalizedSchema,
  filter: PlainObject,
  declaredIndexes: readonly NamedIndexSpec[],
): void {
  // Avoid the work in production AND test. NODE_ENV is set to "test" by
  // most JS test runners (vitest/jest set it automatically; node:test
  // users typically set it explicitly via `NODE_ENV=test npm test`).
  // Skipping in test keeps mock-based suites quiet without disabling the
  // warning where it matters (dev: NODE_ENV unset or "development").
  const nodeEnv = (globalThis as { process?: { env?: { NODE_ENV?: string } } }).process?.env?.NODE_ENV;
  if (nodeEnv === "production") return;
  // In tests we honour an explicit opt-in so the named-indexes suite can
  // observe the warning without forcing every other suite to deal with it.
  const opt = (globalThis as { __zeroshipDbWarnIndexInTest?: boolean }).__zeroshipDbWarnIndexInTest;
  if (nodeEnv === "test" && opt !== true) return;

  if (filter === null || typeof filter !== "object") return;
  const keys = Object.keys(filter).filter(
    (k) => !k.startsWith("$") && k in schema,
  );
  if (keys.length === 0) return;
  // `id` is always the primary key — never warn on it.
  if (keys.length === 1 && keys[0] === "id") return;

  if (_filterCoveredByIndex(keys, schema, declaredIndexes)) return;

  const shapeKey = `${collection}:${[...keys].sort().join(",")}`;
  if (_warnedShapes.has(shapeKey)) return;
  _warnedShapes.add(shapeKey);

  const declaredNames = declaredIndexes.map((i) => i.name);
  const declaredHint = declaredNames.length > 0
    ? `Declared indexes: ${declaredNames.join(", ")}.`
    : "No indexes declared on this collection.";
  // `console.warn` is the standard channel here — matches Convex's
  // ESLint rule shape. We do not throw: this is a nudge, not a hard error.
  console.warn(
    `[@zeroship/db] unindexed query on "${collection}" — ` +
    `filter keys [${keys.join(", ")}] match no declared index. ` +
    `${declaredHint} ` +
    `Add .index("by_X", [${keys.map((k) => JSON.stringify(k)).join(", ")}]) ` +
    `to the schema, or filter by a prefix of an existing index.`,
  );
}

/**
 * True iff the filter keys (in any order) form a non-empty prefix of
 * some declared index, OR every key carries a single-field index marker
 * (`def.index === true` / `def.unique === true`). The schema-level
 * markers are kept as the single-column path so `t.string().unique()`
 * still suppresses the warning without requiring a `.index(...)`
 * declaration.
 */
function _filterCoveredByIndex(
  keys: string[],
  schema: NormalizedSchema,
  declaredIndexes: readonly NamedIndexSpec[],
): boolean {
  // Single-field path: any key with `.index()` / `.unique()` is enough.
  for (const k of keys) {
    const def = schema[k];
    if (def && (def.index === true || def.unique === true)) {
      // The single-field marker covers a filter that uses ONLY that one
      // key, or compound filters where every other key is also indexed.
      // The simplest correct rule: at least one indexed key suffices to
      // trigger an index scan; Postgres can filter the rest. So we
      // accept coverage as soon as one key is marked.
      return true;
    }
  }
  // Multi-column path: keys form a prefix of some declared index.
  const keySet = new Set(keys);
  for (const idx of declaredIndexes) {
    if (idx.fields.length === 0) continue;
    if (keySet.size > idx.fields.length) continue;
    let covers = true;
    for (let i = 0; i < keySet.size; i++) {
      if (!keySet.has(idx.fields[i])) {
        covers = false;
        break;
      }
    }
    if (covers) return true;
  }
  return false;
}

/**
 * Represents a named collection and exposes the full CRUD + aggregate API.
 * The generic parameter `S` is the raw schema shape from which document and input
 * types are derived. `N` carries the table name as a string-literal so the
 * `Id` accessor below produces `Id<N>` rather than `Id<string>`.
 *
 * Use `model()` or `createDb()` — do not construct directly.
 */
export class Collection<S = PlainObject, N extends string = string> {
  private _name: string;
  private _schema: NormalizedSchema;
  private _native: NativeDb;
  /** Lazily resolved Collection v8_class instance — see `_col()`. */
  private _nativeCol: NativeCollection | null;
  private _knownFields: Set<string>;
  private _toColumn: (field: string) => string;
  private _toField: (column: string) => string;
  private _ready: Promise<void> | null;
  private _softDelete: boolean;
  private _versioning: boolean;
  /**
   * Named multi-column indexes declared via `schema(...).index(name, fields)`.
   * Field names are already mapped to column names so the runtime warning
   * compares them against filters that have also been column-mapped.
   */
  private _indexes: readonly NamedIndexSpec[];
  /** Per-collection DataLoader, lazily constructed on first batchable `get(id)`. */
  private _idLoader: IdLoader<Row<S>> | null;
  /**
   * Active-transaction depth. `db.transaction()` wraps `tx.x.*` calls
   * with an increment/decrement so the loader is bypassed while a tx is
   * live on this collection — see `_callWithTx` in db.ts. Mixing a
   * batched read with `TX_CONN`-routed reads in the same microtask
   * would otherwise blur the connection-routing boundary.
   */
  private _txDepth: number;

  /**
   * Type-only handle for `Id<TableName>` — write `typeof db.users.Id` to
   * get a branded `Id<"users">` without rebuilding the name through
   * generic argument inference. At runtime these are non-enumerable
   * `null` properties; the brand exists purely at the type layer (see
   * `Id<T>` in `./types.ts`).
   */
  declare readonly Id: Id<N>;

  /**
   * Type-only handle for `RowInput<S>` — write `typeof db.users.RowInput`
   * to receive the insert-shaped type without `Parameters<...>` plumbing.
   * Mirrors `Id` above; runtime value is `null`.
   */
  declare readonly RowInput: RowInput<S>;

  constructor(name: string, schema: NormalizedSchema, native: NativeDb, options?: { naming?: NamingStrategy; ready?: Promise<void> | null; softDelete?: boolean; versioning?: boolean; indexes?: readonly NamedIndexSpec[] }) {
    this._name = name;
    this._schema = schema;
    this._native = native;
    this._nativeCol = null;
    this._ready = options?.ready ?? null;
    this._softDelete = options?.softDelete ?? false;
    this._versioning = options?.versioning ?? false;
    this._idLoader = null;
    this._txDepth = 0;

    // Build field↔column lookup maps once at init — O(1) at query time
    const strategy = options?.naming ?? naming.asIs;
    const fieldToCol: Record<string, string> = {};
    const colToField: Record<string, string> = {};
    for (const field of Object.keys(schema)) {
      const col = strategy.toColumn(field);
      fieldToCol[field] = col;
      colToField[col] = field;
    }
    const autoFields = ["id", "createdAt", "updatedAt"];
    if (this._softDelete) autoFields.push("deletedAt");
    for (const field of autoFields) {
      const col = strategy.toColumn(field);
      fieldToCol[field] = col;
      colToField[col] = field;
    }
    this._knownFields = new Set(Object.keys(fieldToCol));
    this._toColumn = (field) => fieldToCol[field] ?? field;
    this._toField = (column) => colToField[column] ?? column;

    // The warning path compares declared indexes against unmapped JS
    // field names (matching `_schema` keys + the filter's user-visible
    // shape). Wire-format column mapping happens once at registerModel
    // time in `model()`, not here.
    this._indexes = (options?.indexes ?? []).map((idx) => ({
      name: idx.name,
      fields: [...idx.fields],
      ...(idx.unique ? { unique: true } : {}),
    }));
  }

  /** Await table registration (DDL) before first operation. */
  private async ensureReady(): Promise<void> {
    if (this._ready) {
      await this._ready;
      this._ready = null; // Only await once
    }
  }

  /**
   * Resolve the Collection v8_class instance for this collection name.
   * Cached on first call so subsequent CRUD ops are a single property
   * read. The native runtime exposes `env.db.collection(name)` as a
   * Db v8_method that returns a typed Collection wrapper; calling it
   * twice with the same `name` returns the same JS object (identity is
   * cached on the Db wrapper).
   */
  private _col(): NativeCollection {
    if (this._nativeCol) return this._nativeCol;
    const dbAny = this._native as unknown as { collection?: (n: string) => NativeCollection };
    if (typeof dbAny.collection !== "function") {
      throw new Error(
        "@zeroship/db: env.db.collection(name) not available — " +
        "runtime is missing the Collection v8_class surface.",
      );
    }
    this._nativeCol = dbAny.collection(this._name);
    return this._nativeCol;
  }

  /** @internal — used by `createDb` to chain registrations sequentially
   *  for B2 cross-table FK ordering. Replaces the per-collection `_ready`
   *  promise set during `model()` construction with a chained one so
   *  that parent-table registration completes before child-table
   *  registration starts. */
  _setReady(p: Promise<void> | null): void {
    this._ready = p;
  }

  /** Wraps an operation in ensureReady + try/catch → Result. Eliminates boilerplate per method. */
  private async _run<T>(fn: () => Promise<T>): Promise<Result<T>> {
    try {
      await this.ensureReady();
      return ok(await fn());
    } catch (e) {
      return err(toResultError(e));
    }
  }

  /**
   * D4 — return the caller-supplied `version: N` value from a filter,
   * but only when versioning is enabled on this collection AND the
   * value is a plain number (not a `$gt`/`$in`/etc. operator). Returns
   * `null` otherwise so callers can short-circuit to the non-CAS path.
   */
  private _extractCasVersion(filter: PlainObject): number | null {
    if (!this._versioning) return null;
    if (filter === null || typeof filter !== "object") return null;
    const v = filter.version;
    if (typeof v === "number" && Number.isFinite(v)) return v;
    return null;
  }

  /**
   * D4 — when a CAS version is in play, layer `{ $inc: { version: 1 } }`
   * on top of the user-supplied update so the bump happens atomically
   * inside the same SQL statement as the SET. We merge into any
   * existing `$inc` rather than overwriting.
   */
  private _augmentUpdateWithVersion(update: PlainObject, casVersion: number | null): PlainObject {
    if (casVersion === null) return update;
    const result: PlainObject = { ...update };
    const existingInc = result.$inc;
    const inc =
      existingInc !== null && typeof existingInc === "object" && !Array.isArray(existingInc)
        ? { ...(existingInc as PlainObject), version: 1 }
        : { version: 1 };
    result.$inc = inc;
    return result;
  }

  /**
   * Merges the soft-delete condition into a user-supplied filter.
   * When soft delete is enabled, adds `{ deleted_at: null }` so that
   * soft-deleted documents are invisible to all read operations.
   */
  private _mergeFilter(filter: ZeroshipDbFilter): ZeroshipDbFilter {
    if (!this._softDelete) return filter;
    const softFilter: ZeroshipDbFilter = { [this._toColumn("deletedAt")]: null };
    const hasKeys = Object.keys(filter).length > 0;
    return hasKeys ? { $and: [filter, softFilter] } as ZeroshipDbFilter : softFilter;
  }

  /**
   * Inserts a single row after validating it against the schema.
   * Returns the persisted row with `id`, `createdAt`, and `updatedAt` set.
   */
  async insert(row: RowInput<S>): Promise<Result<Row<S>>> {
    return this._run(async () => {
      const validated = validateDoc(row as PlainObject, this._schema);
      const outbound = mapDocOutbound(validated, this._toColumn);
      const result = await this._col().insert(outbound as Record<string, ZeroshipScalar | ZeroshipScalar[]>);
      return mapResultDoc(result as PlainObject, this._toField) as Row<S>;
    });
  }

  /**
   * Inserts multiple rows after validating each against the schema.
   * Returns the persisted rows with field names mapped to the user-facing shape.
   */
  async insertMany(rows: RowInput<S>[]): Promise<Result<Row<S>[]>> {
    if (rows.length === 0) return ok([] as Row<S>[]);
    return this._run(async () => {
      const validated = (rows as PlainObject[]).map((r) => validateDoc(r, this._schema));
      const outbound = validated.map((r) => mapDocOutbound(r, this._toColumn));
      const results = await this._col().insertMany(outbound as Record<string, ZeroshipScalar | ZeroshipScalar[]>[]);
      return (results ?? []).map((r) => mapResultDoc(r as PlainObject, this._toField)) as Row<S>[];
    });
  }

  /**
   * Fetch a single row. The first argument is either an `id` (a bare
   * `number` or `Id<N>` — branded ids stay narrowed) or a full filter
   * object. When the filter matches multiple rows, `opts.orderBy`
   * decides which one is returned; without an orderBy the choice is
   * undefined. Returns `null` if no row matches.
   *
   * `opts.select` projects to a subset of columns; when the array is
   * typed (`["email", "id"] as const` or a `K[]` literal) the return
   * type narrows to `Pick<Row<S>, K> | null` so projected calls don't
   * have to widen back to `Row<S>`.
   */
  async get<K extends string & keyof Row<S>>(
    idOrFilter: number | Id<N> | Filter<S>,
    opts: { select: K[]; orderBy?: Record<string, 1 | -1> },
  ): Promise<Result<Pick<Row<S>, K> | null>>;
  async get(
    idOrFilter: number | Id<N> | Filter<S>,
    opts?: { orderBy?: Record<string, 1 | -1> },
  ): Promise<Result<Row<S> | null>>;
  async get(
    idOrFilter: number | Id<N> | Filter<S>,
    opts: { select?: (string & keyof Row<S>)[]; orderBy?: Record<string, 1 | -1> } = {},
  ): Promise<Result<Row<S> | null>> {
    trackCollectionAccess(this._name);
    // DataLoader path: a bare numeric id with no projection / ordering
    // and no active tx. Coalesces concurrent `get(id)` calls in one
    // microtask into a single `WHERE id IN (...)` fetch.
    if (
      typeof idOrFilter === "number" &&
      opts.select === undefined &&
      opts.orderBy === undefined &&
      this._txDepth === 0
    ) {
      return this._run(() => this._loadById(idOrFilter));
    }
    const filter = (typeof idOrFilter === "number"
      ? ({ id: idOrFilter } as Filter<S>)
      : idOrFilter);
    if (typeof idOrFilter !== "number") {
      _maybeWarnUnindexedFilter(this._name, this._schema, filter as PlainObject, this._indexes);
    }
    return this._run(async () => {
      const mapped = this._mergeFilter(mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn));
      const nativeOpts: ZeroshipDbFindOpts = {};
      if (opts.select !== undefined) {
        nativeOpts.select = opts.select.map((f) => this._toColumn(f));
      }
      if (opts.orderBy !== undefined) {
        const mappedOrder: Record<string, 1 | -1> = {};
        for (const [k, v] of Object.entries(opts.orderBy)) {
          mappedOrder[this._toColumn(k)] = v as 1 | -1;
        }
        nativeOpts.orderBy = mappedOrder;
      }
      const result = await this._col().findOne(mapped, nativeOpts);
      if (result === null) return null;
      return mapResultDoc(result as PlainObject, this._toField) as Row<S>;
    });
  }

  /** Lazily build the per-collection IdLoader and route the request
   *  through it. The flush callback fires `find({id: {$in: ids}})` via
   *  the native Collection wrapper — this picks up `TX_CONN` routing
   *  in Rust for free, but the caller has already filtered out
   *  tx-active calls so we should never run inside one. */
  private async _loadById(id: number): Promise<Row<S> | null> {
    await this.ensureReady();
    if (this._idLoader === null) {
      this._idLoader = new IdLoader<Row<S>>(async (ids) => {
        const filter: ZeroshipDbFilter = this._mergeFilter(
          mapFilterOutbound(
            { id: { $in: ids } } as unknown as ZeroshipDbFilter,
            this._toColumn,
          ),
        );
        const rows = (await this._col().find(filter, {})) ?? [];
        const map = new Map<number, Row<S>>();
        for (const r of rows) {
          const mapped = mapResultDoc(r as PlainObject, this._toField) as Row<S>;
          map.set(mapped.id, mapped);
        }
        return map;
      });
    }
    return this._idLoader.load(id);
  }

  /** @internal — tx wrapper hook. Increments `_txDepth` so the loader
   *  is bypassed for the duration of the callback. Any pending batch
   *  is drained synchronously into a microtask so a tx-active read
   *  never lands in a non-tx batch. */
  async _withTxBypass<T>(fn: () => Promise<T>): Promise<T> {
    if (this._idLoader !== null) {
      // Fire-and-forget — pending non-tx loads continue against the
      // non-tx connection; the tx call follows on its own dispatch.
      void this._idLoader._drain();
    }
    this._txDepth += 1;
    try {
      return await fn();
    } finally {
      this._txDepth -= 1;
    }
  }

  /**
   * Returns true if at least one document matches `filter`.
   *
   * Implemented as `find(filter).limit(1)` so Postgres can short-circuit
   * on an index scan once a single row matches (vs. a full `COUNT(*)`
   * scan). Cost is bounded by the cost of producing one matching row.
   */
  async exists(filter: Filter<S>): Promise<Result<boolean>> {
    trackCollectionAccess(this._name);
    const { data, error } = await this.find(filter).limit(1);
    if (error) return err(error);
    return ok((data?.length ?? 0) > 0);
  }

  /**
   * Returns a lazy Query that can be chained with `.sort()`, `.limit()`, `.skip()`,
   * and `.select()` before being awaited.
   */
  find(filter: Filter<S> = {} as Filter<S>): Query<S, Row<S>> {
    trackCollectionAccess(this._name);
    _maybeWarnUnindexedFilter(this._name, this._schema, filter as PlainObject, this._indexes);
    const mapped = this._mergeFilter(mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn));
    return new Query<S, Row<S>>(
      this._name,
      mapped,
      async (_col, f, opts) => {
        await this.ensureReady();
        return this._col().find(f, opts);
      },
      this._toField,
      this._toColumn,
    );
  }

  /**
   * Returns the row matching `filter`, or inserts `create` and returns
   * the new row when no match exists. The single SQL statement is
   * `INSERT ... ON CONFLICT DO UPDATE` with a no-op SET; the second
   * return value flags whether the row was freshly created (`true`) or
   * already existed (`false`).
   *
   * `opts.conflictFields` defaults to the unique single-key filter
   * column when the filter has exactly one key and that column carries
   * `.unique()` in the schema. Filters with multiple keys (or a key that
   * isn't unique) require explicit `opts.conflictFields`.
   */
  async findOrCreate(
    filter: Filter<S>,
    create: RowInput<S>,
    opts?: { conflictFields?: (string & keyof Row<S>)[] },
  ): Promise<Result<{ row: Row<S>; created: boolean }>> {
    return this._run(async () => {
      const filterKeys = Object.keys(filter as PlainObject).filter((k) => !k.startsWith("$"));
      let conflictFields = opts?.conflictFields;
      if (conflictFields === undefined || conflictFields.length === 0) {
        if (filterKeys.length !== 1) {
          throw new TypeError(
            `findOrCreate: opts.conflictFields is required when filter has ${filterKeys.length} keys (only single-key unique filters auto-infer)`,
          );
        }
        const k = filterKeys[0];
        const def = this._schema[k];
        if (!def || def.unique !== true) {
          throw new TypeError(
            `findOrCreate: filter key "${k}" is not declared .unique() — pass opts.conflictFields explicitly`,
          );
        }
        conflictFields = [k as string & keyof Row<S>];
      }
      // The persisted document is the filter merged into the create
      // payload — filter values are the identity of the row we want, so
      // they always win over `create` for the conflict columns.
      const merged: PlainObject = { ...(create as PlainObject), ...(filter as PlainObject) };
      const validated = validateDoc(merged, this._schema);
      const outbound = mapDocOutbound(validated, this._toColumn);
      const conflictCols = conflictFields.map((f) => this._toColumn(f as string));
      const colAny = this._col() as unknown as {
        findOrCreate(
          doc: Record<string, ZeroshipScalar | ZeroshipScalar[]>,
          opts: { conflictFields: string[] },
        ): Promise<{ row: Record<string, unknown>; created: boolean }>;
      };
      const raw = await colAny.findOrCreate(
        outbound as Record<string, ZeroshipScalar | ZeroshipScalar[]>,
        { conflictFields: conflictCols },
      );
      const row = mapResultDoc(raw.row as PlainObject, this._toField) as Row<S>;
      return { row, created: raw.created === true };
    });
  }

  /**
   * Inserts a row or updates it if a conflict occurs on the specified fields.
   * Returns the persisted row (either newly inserted or updated).
   */
  async upsert(
    row: RowInput<S>,
    options: { conflictFields: (string & keyof Row<S>)[] }
  ): Promise<Result<Row<S>>> {
    return this._run(async () => {
      const validated = validateDoc(row as PlainObject, this._schema);
      const outbound = mapDocOutbound(validated, this._toColumn);
      const conflictCols = options.conflictFields.map((f) => this._toColumn(f));
      const result = await this._col().upsert(
        outbound as Record<string, ZeroshipScalar | ZeroshipScalar[]>,
        { conflictFields: conflictCols },
      );
      return mapResultDoc(result as PlainObject, this._toField) as Row<S>;
    });
  }

  /**
   * Updates the first document matching `idOrFilter` and returns the
   * updated document (or `null` if nothing matched). When the first
   * argument is a number it is treated as `{ id: <n> }`; otherwise a
   * full filter is accepted (e.g. compound filters for optimistic
   * concurrency: `{ id, version: 3 }`).
   *
   * `patch` accepts either MongoDB-style operators
   * (`{ $set: {...}, $inc: { n: 1 } }`) or a bare field map (treated as
   * `$set`). The fields are validated against the schema; array push
   * operations are validated against the declared item type.
   * Returns the updated row (or `null` if no row matched).
   */
  async update(
    idOrFilter: number | Filter<S>,
    patch: UpdateExpression<S>
  ): Promise<Result<Row<S> | null>> {
    return this._run(async () => {
      const filter = (typeof idOrFilter === "number"
        ? ({ id: idOrFilter } as Filter<S>)
        : idOrFilter);
      const updateObj = patch as PlainObject;
      const fields = extractUpdateFields(updateObj);
      checkPartial(fields, this._schema);
      validateArrayPushOps(updateObj, this._schema);
      // D4 — extract `version: N` from the filter when versioning is on
      // and use it as a CAS guard. The patch is augmented with $inc:1
      // on `version` so the increment happens atomically with the SET.
      const casVersion = this._extractCasVersion(filter as PlainObject);
      const augmentedUpdate = this._augmentUpdateWithVersion(updateObj, casVersion);
      const mappedFilter = mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn);
      const mappedUpdate = mapUpdateOutbound(augmentedUpdate, this._toColumn);
      const result = await this._col().updateOne(mappedFilter, mappedUpdate);
      if (result === null) {
        if (casVersion !== null) {
          throw new OptimisticLockError(casVersion, this._name);
        }
        return null;
      }
      return mapResultDoc(result as PlainObject, this._toField) as Row<S>;
    });
  }

  /**
   * Updates all documents matching `filter` using the given `update`.
   * Validates the fields in `$set` and bare keys against the schema.
   * Validates `$push`/`$addToSet` values against the declared array item type.
   * Returns `{ count }` — the number of rows affected. The legacy
   * `{ matchedCount, modifiedCount }` shape always carried identical
   * values (the native layer only reports one count); collapsing to a
   * single field is cleaner.
   */
  async updateMany(
    filter: Filter<S>,
    update: UpdateExpression<S>
  ): Promise<Result<{ count: number }>> {
    return this._run(async () => {
      const updateObj = update as PlainObject;
      const fields = extractUpdateFields(updateObj);
      checkPartial(fields, this._schema);
      validateArrayPushOps(updateObj, this._schema);
      const casVersion = this._extractCasVersion(filter as PlainObject);
      const augmentedUpdate = this._augmentUpdateWithVersion(updateObj, casVersion);
      const mappedFilter = mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn);
      const mappedUpdate = mapUpdateOutbound(augmentedUpdate, this._toColumn);
      const n = await this._col().updateMany(mappedFilter, mappedUpdate);
      if (n === 0 && casVersion !== null) {
        throw new OptimisticLockError(casVersion, this._name);
      }
      return { count: n };
    });
  }

  /**
   * Deletes the first document matching `idOrFilter` and returns the
   * deleted document (or `null` if nothing matched). When the first
   * argument is a number it is treated as `{ id: <n> }`.
   *
   * When the collection has `softDelete: true`, this sets `deletedAt`
   * instead of removing the row; pass `{ hard: true }` to bypass and
   * permanently remove the row.
   */
  async delete(
    idOrFilter: number | Filter<S>,
    opts: { hard?: boolean } = {},
  ): Promise<Result<Row<S> | null>> {
    return this._run(async () => {
      const filter = (typeof idOrFilter === "number"
        ? ({ id: idOrFilter } as Filter<S>)
        : idOrFilter);
      const hard = opts.hard === true;
      // Both branches honor CAS — versioning + `{ version: N }` in the
      // filter must reject a concurrent writer's lost update, soft or
      // hard. The patch on the soft-delete branch bumps version too.
      const casVersion = this._extractCasVersion(filter as PlainObject);
      if (this._softDelete && !hard) {
        const mapped = this._mergeFilter(mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn));
        const col = this._toColumn("deletedAt");
        const patch = this._augmentUpdateWithVersion(
          { [col]: Date.now() } as PlainObject,
          casVersion,
        );
        const result = await this._col().updateOne(mapped, patch as ZeroshipDbUpdate);
        if (result === null) {
          if (casVersion !== null) throw new OptimisticLockError(casVersion, this._name);
          return null;
        }
        return mapResultDoc(result as PlainObject, this._toField) as Row<S>;
      }
      const mapped = mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn);
      const result = await this._col().deleteOne(mapped);
      if (result === null) {
        if (casVersion !== null) throw new OptimisticLockError(casVersion, this._name);
        return null;
      }
      return mapResultDoc(result as PlainObject, this._toField) as Row<S>;
    });
  }

  /**
   * Deletes all documents matching `filter`. Returns
   * `{ deletedCount: N }`. When the collection has `softDelete: true`,
   * sets `deletedAt` on each row; pass `{ hard: true }` to bypass.
   */
  async deleteMany(
    filter: Filter<S>,
    opts: { hard?: boolean } = {},
  ): Promise<Result<{ deletedCount: number }>> {
    _maybeWarnUnindexedFilter(this._name, this._schema, filter as PlainObject, this._indexes);
    return this._run(async () => {
      const hard = opts.hard === true;
      const casVersion = this._extractCasVersion(filter as PlainObject);
      if (this._softDelete && !hard) {
        const mapped = this._mergeFilter(mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn));
        const col = this._toColumn("deletedAt");
        const patch = this._augmentUpdateWithVersion(
          { [col]: Date.now() } as PlainObject,
          casVersion,
        );
        const n = await this._col().updateMany(mapped, patch as ZeroshipDbUpdate);
        if (n === 0 && casVersion !== null) throw new OptimisticLockError(casVersion, this._name);
        return { deletedCount: n };
      }
      const mapped = mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn);
      const n = await this._col().deleteMany(mapped);
      if (n === 0 && casVersion !== null) throw new OptimisticLockError(casVersion, this._name);
      return { deletedCount: n };
    });
  }

  /**
   * Counts documents matching `filter`. Defaults to counting all documents when
   * no filter is provided.
   */
  async count(filter: Filter<S> = {} as Filter<S>): Promise<Result<number>> {
    trackCollectionAccess(this._name);
    return this._run(async () => {
      const mapped = this._mergeFilter(mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn));
      const n = await this._col().count(mapped);
      return typeof n === "number" ? n : 0;
    });
  }

  /**
   * Returns the unique values of `field` across documents matching `filter`.
   * Defaults to all documents when no filter is provided.
   */
  async distinct(field: string & keyof Row<S>, filter: Filter<S> = {} as Filter<S>): Promise<Result<(string | number | boolean | null)[]>> {
    trackCollectionAccess(this._name);
    return this._run(async () => {
      if (!this._knownFields.has(field)) {
        throw new ValidationError({ [field]: { path: field, message: `unknown field: ${field}` } });
      }
      const mapped = this._mergeFilter(mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn));
      const column = this._toColumn(field);
      const result = await this._col().distinct(mapped, { field: column });
      return result ?? [];
    });
  }

  /**
   * Runs an aggregation pipeline (MongoDB-style) and returns the mapped results.
   * `$group`, `$match`, and accumulator expressions are translated to the native format.
   */
  async aggregate(pipeline: ZeroshipDbAggregateStage[]): Promise<Result<PlainObject[]>> {
    trackCollectionAccess(this._name);
    return this._run(async () => {
      let effectivePipeline: ZeroshipDbAggregateStage[] = pipeline;
      if (this._softDelete) {
        const softFilter: ZeroshipDbFilter = { [this._toColumn("deletedAt")]: null };
        const head = pipeline[0] as { $match?: ZeroshipDbFilter } | undefined;
        if (head && head.$match !== undefined) {
          const existing = head.$match;
          effectivePipeline = [
            { $match: { $and: [existing, softFilter] } as ZeroshipDbFilter },
            ...pipeline.slice(1),
          ];
        } else {
          effectivePipeline = [{ $match: softFilter }, ...pipeline];
        }
      }
      const translated = translateAggregatePipeline(
        effectivePipeline as unknown as PlainObject[],
        this._toColumn,
      ) as ZeroshipDbAggregateStage[];
      const results = await this._col().aggregate(translated);
      return (results ?? []).map((d) => mapResultDoc(d as PlainObject, this._toField));
    });
  }

}
