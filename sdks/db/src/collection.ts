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
import { PlainObject, Result, Row, RowInput, UpdateExpression, Filter, type Id, type NamingStrategy, naming, ok, err } from "./types.js";

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
  else {
    const msg = e instanceof Error ? e.message : String(e);
    out = mapNativeError(msg);
  }
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

      let ok = true;
      if (itemType === "string") ok = typeof val === "string";
      else if (itemType === "number") ok = typeof val === "number";
      else if (itemType === "boolean") ok = typeof val === "boolean";
      else if (itemType === "date") ok = val instanceof Date || typeof val === "string";

      if (!ok) {
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
 * scan because no key in it has an `index: true` / `unique: true` marker
 * in the normalized schema. Only fires when `process.env.NODE_ENV !==
 * "production"`. Deduplicates by `${collection}:${sortedKeys}` so noisy
 * code paths don't spam.
 *
 * The heuristic is intentionally simple: every top-level key in the
 * filter that maps to a schema field is checked; if none of them is
 * indexed and at least one is a single-field equality, we warn. False
 * positives are acceptable; weighting by selectivity is a future
 * refinement.
 */
function _maybeWarnUnindexedFilter(
  collection: string,
  schema: NormalizedSchema,
  filter: PlainObject,
): void {
  // Avoid the work in production AND test. NODE_ENV is set to "test" by
  // most JS test runners (vitest/jest set it automatically; node:test
  // users typically set it explicitly via `NODE_ENV=test npm test`).
  // Skipping in test keeps mock-based suites quiet without disabling the
  // warning where it matters (dev: NODE_ENV unset or "development").
  const nodeEnv = (globalThis as { process?: { env?: { NODE_ENV?: string } } }).process?.env?.NODE_ENV;
  if (nodeEnv === "production" || nodeEnv === "test") return;

  if (filter === null || typeof filter !== "object") return;
  const keys = Object.keys(filter).filter(
    (k) => !k.startsWith("$") && k in schema,
  );
  if (keys.length === 0) return;
  // `id` is always the primary key — never warn on it.
  if (keys.length === 1 && (keys[0] === "id" || keys[0] === "_id")) return;

  let anyIndexed = false;
  for (const k of keys) {
    const def = schema[k];
    if (def && (def.index === true || def.unique === true)) {
      anyIndexed = true;
      break;
    }
  }
  if (anyIndexed) return;

  const shapeKey = `${collection}:${[...keys].sort().join(",")}`;
  if (_warnedShapes.has(shapeKey)) return;
  _warnedShapes.add(shapeKey);

  const hint = keys
    .map((k) => `t.<type>().index() on ${collection}.${k}`)
    .join(" or ");
  // `console.warn` is the standard channel here — matches Convex's
  // ESLint rule shape. We do not throw: this is a nudge, not a hard error.
  console.warn(
    `[@zeroship/db] unindexed query on "${collection}" — ` +
    `filter keys [${keys.join(", ")}] have no index. ` +
    `Consider adding ${hint}.`,
  );
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

  constructor(name: string, schema: NormalizedSchema, native: NativeDb, options?: { naming?: NamingStrategy; ready?: Promise<void> | null; softDelete?: boolean; versioning?: boolean }) {
    this._name = name;
    this._schema = schema;
    this._native = native;
    this._nativeCol = null;
    this._ready = options?.ready ?? null;
    this._softDelete = options?.softDelete ?? false;
    this._versioning = options?.versioning ?? false;

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
   * `opts.select` projects to a subset of columns.
   */
  async get(
    idOrFilter: number | Id<N> | Filter<S>,
    opts: { select?: (string & keyof Row<S>)[]; orderBy?: Record<string, 1 | -1> } = {},
  ): Promise<Result<Row<S> | null>> {
    const filter = (typeof idOrFilter === "number"
      ? ({ id: idOrFilter } as Filter<S>)
      : idOrFilter);
    if (typeof idOrFilter !== "number") {
      _maybeWarnUnindexedFilter(this._name, this._schema, filter as PlainObject);
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

  /**
   * Returns true if at least one document matches `filter`.
   *
   * **Cost note** — this is implemented as `count(filter) > 0`, which
   * scans every matching row (Postgres has no short-circuit `EXISTS`
   * on the native primitive yet). Pass a tight filter (indexed
   * equality, narrow time range, etc.) for predictable cost; for large
   * tables, an unfiltered `exists({})` is a full-table count and will
   * be slow.
   */
  async exists(filter: Filter<S>): Promise<Result<boolean>> {
    const { data, error } = await this.count(filter);
    if (error) return err(error);
    return ok((data ?? 0) > 0);
  }

  /**
   * Returns a lazy Query that can be chained with `.sort()`, `.limit()`, `.skip()`,
   * and `.select()` before being awaited.
   */
  find(filter: Filter<S> = {} as Filter<S>): Query<S, Row<S>> {
    _maybeWarnUnindexedFilter(this._name, this._schema, filter as PlainObject);
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
   * Returns `{ matchedCount, modifiedCount }` indicating whether a document was found.
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
   * Returns `{ matchedCount, modifiedCount }` with the count from the native layer.
   */
  async updateMany(
    filter: Filter<S>,
    update: UpdateExpression<S>
  ): Promise<Result<{ matchedCount: number; modifiedCount: number }>> {
    return this._run(async () => {
      const updateObj = update as PlainObject;
      const fields = extractUpdateFields(updateObj);
      checkPartial(fields, this._schema);
      validateArrayPushOps(updateObj, this._schema);
      // D4 — same CAS handling as updateOne. updateMany with a CAS
      // version still increments `version` on every matched row.
      const casVersion = this._extractCasVersion(filter as PlainObject);
      const augmentedUpdate = this._augmentUpdateWithVersion(updateObj, casVersion);
      const mappedFilter = mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn);
      const mappedUpdate = mapUpdateOutbound(augmentedUpdate, this._toColumn);
      const n = await this._col().updateMany(mappedFilter, mappedUpdate);
      if (n === 0 && casVersion !== null) {
        throw new OptimisticLockError(casVersion, this._name);
      }
      return { matchedCount: n, modifiedCount: n };
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
      if (this._softDelete && !hard) {
        const mapped = this._mergeFilter(mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn));
        const col = this._toColumn("deletedAt");
        const result = await this._col().updateOne(mapped, { [col]: Date.now() as ZeroshipDbUpdateValue });
        return result === null ? null : (mapResultDoc(result as PlainObject, this._toField) as Row<S>);
      }
      const mapped = mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn);
      const result = await this._col().deleteOne(mapped);
      return result === null ? null : (mapResultDoc(result as PlainObject, this._toField) as Row<S>);
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
    _maybeWarnUnindexedFilter(this._name, this._schema, filter as PlainObject);
    return this._run(async () => {
      const hard = opts.hard === true;
      if (this._softDelete && !hard) {
        const mapped = this._mergeFilter(mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn));
        const col = this._toColumn("deletedAt");
        const n = await this._col().updateMany(mapped, { [col]: Date.now() as ZeroshipDbUpdateValue });
        return { deletedCount: n };
      }
      const mapped = mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn);
      const n = await this._col().deleteMany(mapped);
      return { deletedCount: n };
    });
  }

  /**
   * Counts documents matching `filter`. Defaults to counting all documents when
   * no filter is provided.
   */
  async count(filter: Filter<S> = {} as Filter<S>): Promise<Result<number>> {
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
