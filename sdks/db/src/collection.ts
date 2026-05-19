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
import { PlainObject, Result, Document, CreateInput, UpdateExpression, Filter, type NamingStrategy, naming, ok, err } from "./types.js";

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

/** Parses a raw JSON string (or already-parsed value) from the native layer. */
function parseRaw<T>(raw: string | null | undefined): T | null {
  if (raw === null || raw === undefined) return null;
  let parsed: unknown;
  if (typeof raw === "string") {
    if (raw === "") return null;
    try {
      parsed = JSON.parse(raw);
    } catch (e: unknown) {
      throw new Error(`failed to parse native response: ${e instanceof Error ? e.message : String(e)}`, { cause: e });
    }
  } else {
    parsed = raw;
  }
  // Detect native error envelope: Rust resolves with { "error": "..." } instead of rejecting
  if (parsed && typeof parsed === "object" && "error" in parsed && typeof (parsed as PlainObject).error === "string") {
    throw new Error((parsed as PlainObject).error as string);
  }
  return parsed as T;
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
 * positives are acceptable for V1 (TODO: weight selectivity).
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
 * types are derived. Use `model()` or `createDb()` — do not construct directly.
 */
export class Collection<S = PlainObject> {
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
   * Inserts a single document after validating it against the schema.
   * Returns the persisted document with `id`, `createdAt`, and `updatedAt` mapped.
   */
  async create(doc: CreateInput<S>): Promise<Result<Document<S>>> {
    return this._run(async () => {
      const validated = validateDoc(doc as PlainObject, this._schema);
      const outbound = mapDocOutbound(validated, this._toColumn);
      const raw = await this._col().insert( outbound as Record<string, ZeroshipScalar | ZeroshipScalar[]>);
      const result = parseRaw<PlainObject>(raw);
      return mapResultDoc(result!, this._toField) as Document<S>;
    });
  }

  /**
   * Inserts multiple documents after validating each one against the schema.
   * Returns the persisted documents with field names mapped to the user-facing shape.
   */
  async insertMany(docs: CreateInput<S>[]): Promise<Result<Document<S>[]>> {
    if (docs.length === 0) return ok([] as Document<S>[]);
    return this._run(async () => {
      const validated = (docs as PlainObject[]).map((doc) => validateDoc(doc, this._schema));
      const outbound = validated.map(d => mapDocOutbound(d, this._toColumn));
      const raw = await this._col().insertMany( outbound as Record<string, ZeroshipScalar | ZeroshipScalar[]>[]);
      const results = parseRaw<PlainObject[]>(raw);
      return (results ?? []).map(d => mapResultDoc(d, this._toField)) as Document<S>[];
    });
  }

  /**
   * Finds and returns the first document matching `filter`, or `null` if none exists.
   * Field names in `filter` are mapped outbound before the native call.
   */
  async findOne(filter: Filter<S>): Promise<Result<Document<S> | null>> {
    _maybeWarnUnindexedFilter(this._name, this._schema, filter as PlainObject);
    return this._run(async () => {
      const mapped = this._mergeFilter(mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn));
      const raw = await this._col().findOne( mapped);
      if (raw === null) return null;
      const result = parseRaw<PlainObject>(raw);
      if (result === null) return null;
      return mapResultDoc(result, this._toField) as Document<S>;
    });
  }

  /** Shorthand for `findOne({ id })`. */
  async findById(id: number): Promise<Result<Document<S> | null>> {
    return this.findOne({ id } as Filter<S>);
  }

  /** Returns true if at least one document matches `filter`. */
  async exists(filter: Filter<S>): Promise<Result<boolean>> {
    const { data, error } = await this.countDocuments(filter);
    if (error) return err(error);
    return ok((data ?? 0) > 0);
  }

  /**
   * Returns a lazy Query that can be chained with `.sort()`, `.limit()`, `.skip()`,
   * and `.select()` before being awaited.
   */
  find(filter: Filter<S> = {} as Filter<S>): Query<S, Document<S>> {
    _maybeWarnUnindexedFilter(this._name, this._schema, filter as PlainObject);
    const mapped = this._mergeFilter(mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn));
    return new Query<S, Document<S>>(
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
   * Inserts a document or updates it if a conflict occurs on the specified fields.
   * Returns the persisted document (either newly inserted or updated).
   */
  async upsert(
    doc: CreateInput<S>,
    options: { conflictFields: (string & keyof Document<S>)[] }
  ): Promise<Result<Document<S>>> {
    return this._run(async () => {
      const validated = validateDoc(doc as PlainObject, this._schema);
      const outbound = mapDocOutbound(validated, this._toColumn);
      const conflictCols = options.conflictFields.map((f) => this._toColumn(f));
      const raw = await this._col().upsert(
        outbound as Record<string, ZeroshipScalar | ZeroshipScalar[]>,
        conflictCols,
      );
      const result = parseRaw<PlainObject>(raw);
      return mapResultDoc(result!, this._toField) as Document<S>;
    });
  }

  /**
   * Updates the first document matching `filter` using the given `update`.
   * Validates the fields in `$set` and bare (non-`$`) keys against the schema.
   * Validates `$push`/`$addToSet` values against the declared array item type.
   * Returns `{ matchedCount, modifiedCount }` indicating whether a document was found.
   */
  async updateOne(
    filter: Filter<S>,
    update: UpdateExpression<S>
  ): Promise<Result<{ matchedCount: number; modifiedCount: number }>> {
    return this._run(async () => {
      const updateObj = update as PlainObject;
      const fields = extractUpdateFields(updateObj);
      checkPartial(fields, this._schema);
      validateArrayPushOps(updateObj, this._schema);
      // D4 — extract `version: N` from the filter when versioning is on
      // and use it as a CAS guard. The update is augmented with $inc:1
      // on `version` so the increment happens atomically with the SET.
      const casVersion = this._extractCasVersion(filter as PlainObject);
      const augmentedUpdate = this._augmentUpdateWithVersion(updateObj, casVersion);
      const mappedFilter = mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn);
      const mappedUpdate = mapUpdateOutbound(augmentedUpdate, this._toColumn);
      const raw = await this._col().updateOne( mappedFilter, mappedUpdate);
      const result = parseRaw<PlainObject>(raw);
      const matched = result !== null ? 1 : 0;
      if (matched === 0 && casVersion !== null) {
        throw new OptimisticLockError(casVersion, this._name);
      }
      return { matchedCount: matched, modifiedCount: matched };
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
      const raw = await this._col().updateMany( mappedFilter, mappedUpdate);
      const result = parseRaw<{ updated: number }>(raw);
      const n = result?.updated ?? 0;
      if (n === 0 && casVersion !== null) {
        throw new OptimisticLockError(casVersion, this._name);
      }
      return { matchedCount: n, modifiedCount: n };
    });
  }

  /**
   * Updates the first document matching `filter` and returns the updated document.
   * Returns `null` if no document matches the filter.
   * Same validation as updateOne: validates $set fields and $push/$addToSet ops.
   */
  async findOneAndUpdate(
    filter: Filter<S>,
    update: UpdateExpression<S>
  ): Promise<Result<Document<S> | null>> {
    return this._run(async () => {
      const updateObj = update as PlainObject;
      const fields = extractUpdateFields(updateObj);
      checkPartial(fields, this._schema);
      validateArrayPushOps(updateObj, this._schema);
      // D4 — apply the same CAS augmentation as updateOne. A no-match
      // when a version guard was supplied still surfaces as a typed
      // OptimisticLockError instead of `null` so callers can branch.
      const casVersion = this._extractCasVersion(filter as PlainObject);
      const augmentedUpdate = this._augmentUpdateWithVersion(updateObj, casVersion);
      const mappedFilter = mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn);
      const mappedUpdate = mapUpdateOutbound(augmentedUpdate, this._toColumn);
      const raw = await this._col().updateOne( mappedFilter, mappedUpdate);
      const result = parseRaw<PlainObject>(raw);
      if (result === null) {
        if (casVersion !== null) {
          throw new OptimisticLockError(casVersion, this._name);
        }
        return null;
      }
      return mapResultDoc(result, this._toField) as Document<S>;
    });
  }

  /**
   * Deletes the first document matching `filter` and returns the deleted document.
   * When soft delete is enabled, sets `deleted_at` and returns the document.
   * Returns `null` if no document matches the filter.
   */
  async findOneAndDelete(filter: Filter<S>): Promise<Result<Document<S> | null>> {
    return this._run(async () => {
      if (this._softDelete) {
        const mapped = this._mergeFilter(mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn));
        const col = this._toColumn("deletedAt");
        const raw = await this._col().updateOne( mapped, { [col]: Date.now() as ZeroshipDbUpdateValue });
        const result = parseRaw<PlainObject>(raw);
        return result === null ? null : mapResultDoc(result, this._toField) as Document<S>;
      }
      const mapped = mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn);
      const raw = await this._col().deleteOne( mapped);
      const result = parseRaw<PlainObject>(raw);
      return result === null ? null : mapResultDoc(result, this._toField) as Document<S>;
    });
  }

  /**
   * Deletes the first document matching `filter`.
   * When soft delete is enabled, sets `deleted_at` instead of removing the row.
   * Returns `{ deletedCount: 1 }` if a document was found, `{ deletedCount: 0 }` otherwise.
   */
  async deleteOne(filter: Filter<S>): Promise<Result<{ deletedCount: number }>> {
    return this._run(async () => {
      if (this._softDelete) {
        const mapped = this._mergeFilter(mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn));
        const col = this._toColumn("deletedAt");
        const raw = await this._col().updateOne( mapped, { [col]: Date.now() as ZeroshipDbUpdateValue });
        const result = parseRaw<PlainObject>(raw);
        return { deletedCount: result !== null ? 1 : 0 };
      }
      const mapped = mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn);
      const raw = await this._col().deleteOne( mapped);
      return { deletedCount: raw !== null && raw !== undefined && raw !== "" ? 1 : 0 };
    });
  }

  /**
   * Deletes all documents matching `filter`.
   * When soft delete is enabled, sets `deleted_at` instead of removing rows.
   * Returns `{ deletedCount: N }` where N is the number of documents removed.
   */
  async deleteMany(filter: Filter<S>): Promise<Result<{ deletedCount: number }>> {
    _maybeWarnUnindexedFilter(this._name, this._schema, filter as PlainObject);
    return this._run(async () => {
      if (this._softDelete) {
        const mapped = this._mergeFilter(mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn));
        const col = this._toColumn("deletedAt");
        const raw = await this._col().updateMany( mapped, { [col]: Date.now() as ZeroshipDbUpdateValue });
        const result = parseRaw<{ updated: number }>(raw);
        return { deletedCount: result?.updated ?? 0 };
      }
      const mapped = mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn);
      const raw = await this._col().deleteMany( mapped);
      const result = parseRaw<{ deleted: number }>(raw);
      return { deletedCount: result?.deleted ?? 0 };
    });
  }

  /**
   * Counts documents matching `filter`. Defaults to counting all documents when
   * no filter is provided.
   */
  async countDocuments(filter: Filter<S> = {} as Filter<S>): Promise<Result<number>> {
    return this._run(async () => {
      const mapped = this._mergeFilter(mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn));
      const raw = await this._col().count( mapped);
      const result = parseRaw<{ count: number }>(raw);
      return result?.count ?? 0;
    });
  }

  /**
   * Returns the unique values of `field` across documents matching `filter`.
   * Defaults to all documents when no filter is provided.
   */
  async distinct(field: string & keyof Document<S>, filter: Filter<S> = {} as Filter<S>): Promise<Result<(string | number | boolean | null)[]>> {
    return this._run(async () => {
      if (!this._knownFields.has(field)) {
        throw new ValidationError({ [field]: { path: field, message: `unknown field: ${field}` } });
      }
      const mapped = this._mergeFilter(mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn));
      const column = this._toColumn(field);
      const raw = await this._col().distinct( column, mapped);
      const result = parseRaw<(string | number | boolean | null)[]>(raw);
      return result ?? [];
    });
  }

  /**
   * Runs an aggregation pipeline (MongoDB-style) and returns the mapped results.
   * `$group`, `$match`, and accumulator expressions are translated to the native format.
   */
  async aggregate(pipeline: PlainObject[]): Promise<Result<PlainObject[]>> {
    return this._run(async () => {
      let effectivePipeline = pipeline;
      if (this._softDelete) {
        const softFilter = { [this._toColumn("deletedAt")]: null };
        const hasLeadingMatch = pipeline.length > 0 && "$match" in pipeline[0];
        if (hasLeadingMatch) {
          const existing = pipeline[0].$match as ZeroshipDbFilter;
          effectivePipeline = [{ $match: { $and: [existing, softFilter] } as ZeroshipDbFilter }, ...pipeline.slice(1)];
        } else {
          effectivePipeline = [{ $match: softFilter }, ...pipeline];
        }
      }
      const translated = translateAggregatePipeline(effectivePipeline, this._toColumn) as ZeroshipDbAggregateStage[];
      const raw = await this._col().aggregate( translated);
      const results = parseRaw<PlainObject[]>(raw);
      return (results ?? []).map(d => mapResultDoc(d, this._toField));
    });
  }

  /**
   * Permanently deletes the first document matching `filter`, bypassing soft delete.
   * Always performs a real DELETE regardless of the soft-delete setting.
   */
  async forceDelete(filter: Filter<S>): Promise<Result<{ deletedCount: number }>> {
    return this._run(async () => {
      const mapped = mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn);
      const raw = await this._col().deleteOne( mapped);
      return { deletedCount: raw !== null && raw !== undefined && raw !== "" ? 1 : 0 };
    });
  }

  /**
   * Permanently deletes all documents matching `filter`, bypassing soft delete.
   * Always performs a real DELETE regardless of the soft-delete setting.
   */
  async forceDeleteMany(filter: Filter<S>): Promise<Result<{ deletedCount: number }>> {
    return this._run(async () => {
      const mapped = mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn);
      const raw = await this._col().deleteMany( mapped);
      const result = parseRaw<{ deleted: number }>(raw);
      return { deletedCount: result?.deleted ?? 0 };
    });
  }
}
