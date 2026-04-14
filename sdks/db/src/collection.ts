/**
 * Collection: the main entry point for CRUD operations on a named collection.
 * Each method validates inputs against the schema, maps field names to the native
 * format, calls the native driver, and maps results back to the user-facing shape.
 */
import { NormalizedSchema } from "./schema.js";
import { validateDoc, checkPartial } from "./validate.js";
import { mapNativeError, ValidationError } from "./errors.js";
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


/**
 * Converts a caught value to an Error for inclusion in a Result.
 * ValidationError instances are returned as-is (they are already well-typed).
 * All other errors are passed through mapNativeError so that, e.g., unique
 * constraint violations receive code 11000.
 */
function toResultError(e: unknown): Error {
  if (e instanceof ValidationError) return e;
  const msg = e instanceof Error ? e.message : String(e);
  return mapNativeError(msg);
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
 * Represents a named collection and exposes the full CRUD + aggregate API.
 * The generic parameter `S` is the raw schema shape from which document and input
 * types are derived. Use `model()` or `createDb()` — do not construct directly.
 */
export class Collection<S = PlainObject> {
  private _name: string;
  private _schema: NormalizedSchema;
  private _native: NativeDb;
  private _knownFields: Set<string>;
  private _toColumn: (field: string) => string;
  private _toField: (column: string) => string;
  private _ready: Promise<void> | null;

  constructor(name: string, schema: NormalizedSchema, native: NativeDb, options?: { naming?: NamingStrategy; ready?: Promise<void> | null }) {
    this._name = name;
    this._schema = schema;
    this._native = native;
    this._ready = options?.ready ?? null;

    // Build field↔column lookup maps once at init — O(1) at query time
    const strategy = options?.naming ?? naming.asIs;
    const fieldToCol: Record<string, string> = {};
    const colToField: Record<string, string> = {};
    for (const field of Object.keys(schema)) {
      const col = strategy.toColumn(field);
      fieldToCol[field] = col;
      colToField[col] = field;
    }
    for (const field of ["id", "createdAt", "updatedAt"]) {
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
   * Inserts a single document after validating it against the schema.
   * Returns the persisted document with `_id`, `createdAt`, and `updatedAt` mapped.
   */
  async create(doc: CreateInput<S>): Promise<Result<Document<S>>> {
    try {
      await this.ensureReady();
      const validated = validateDoc(doc as PlainObject, this._schema);
      const outbound = mapDocOutbound(validated, this._toColumn);
      const raw = await this._native.insert(this._name, outbound as Record<string, ZeroshipScalar | ZeroshipScalar[]>);
      const result = parseRaw<PlainObject>(raw);
      return ok(mapResultDoc(result!, this._toField) as Document<S>);
    } catch (e) {
      return err(toResultError(e));
    }
  }

  /**
   * Inserts multiple documents after validating each one against the schema.
   * Returns the persisted documents with field names mapped to the user-facing shape.
   */
  async insertMany(docs: CreateInput<S>[]): Promise<Result<Document<S>[]>> {
    if (docs.length === 0) return ok([] as Document<S>[]);
    try {
      await this.ensureReady();
      const validated = (docs as PlainObject[]).map((doc) => validateDoc(doc, this._schema));
      const outbound = validated.map(d => mapDocOutbound(d, this._toColumn));
      const raw = await this._native.insertMany(this._name, outbound as Record<string, ZeroshipScalar | ZeroshipScalar[]>[]);
      const results = parseRaw<PlainObject[]>(raw);
      return ok((results ?? []).map(d => mapResultDoc(d, this._toField)) as Document<S>[]);
    } catch (e) {
      return err(toResultError(e));
    }
  }

  /**
   * Finds and returns the first document matching `filter`, or `null` if none exists.
   * Field names in `filter` are mapped outbound before the native call.
   */
  async findOne(filter: Filter<S>): Promise<Result<Document<S> | null>> {
    try {
      await this.ensureReady();
      const mapped = mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn);
      const raw = await this._native.findOne(this._name, mapped);
      if (raw === null) return ok(null);
      const result = parseRaw<PlainObject>(raw);
      if (result === null) return ok(null);
      return ok(mapResultDoc(result, this._toField) as Document<S>);
    } catch (e) {
      return err(toResultError(e));
    }
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
  find(filter: Filter<S> = {} as Filter<S>): Query<S> {
    const mapped = mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn);
    return new Query<S>(
      this._name,
      mapped,
      async (col, f, opts) => {
        await this.ensureReady();
        return this._native.find(col, f, opts);
      },
      this._toField
    );
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
    try {
      await this.ensureReady();
      const updateObj = update as PlainObject;
      const fields = extractUpdateFields(updateObj);
      checkPartial(fields, this._schema);
      validateArrayPushOps(updateObj, this._schema);
      const mappedFilter = mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn);
      const mappedUpdate = mapUpdateOutbound(updateObj, this._toColumn);
      const raw = await this._native.updateOne(
        this._name,
        mappedFilter,
        mappedUpdate
      );
      const result = parseRaw<PlainObject>(raw);
      const matched = result !== null ? 1 : 0;
      return ok({ matchedCount: matched, modifiedCount: matched });
    } catch (e) {
      return err(toResultError(e));
    }
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
    try {
      await this.ensureReady();
      const updateObj = update as PlainObject;
      const fields = extractUpdateFields(updateObj);
      checkPartial(fields, this._schema);
      validateArrayPushOps(updateObj, this._schema);
      const mappedFilter = mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn);
      const mappedUpdate = mapUpdateOutbound(updateObj, this._toColumn);
      const raw = await this._native.updateMany(
        this._name,
        mappedFilter,
        mappedUpdate
      );
      const result = parseRaw<{ updated: number }>(raw);
      const n = result?.updated ?? 0;
      return ok({ matchedCount: n, modifiedCount: n });
    } catch (e) {
      return err(toResultError(e));
    }
  }

  /**
   * Deletes the first document matching `filter`.
   * Returns `{ deletedCount: 1 }` if a document was found, `{ deletedCount: 0 }` otherwise.
   */
  async deleteOne(filter: Filter<S>): Promise<Result<{ deletedCount: number }>> {
    try {
      await this.ensureReady();
      const mapped = mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn);
      const raw = await this._native.deleteOne(this._name, mapped);
      return ok({ deletedCount: raw !== null && raw !== undefined && raw !== "" ? 1 : 0 });
    } catch (e) {
      return err(toResultError(e));
    }
  }

  /**
   * Deletes all documents matching `filter`.
   * Returns `{ deletedCount: N }` where N is the number of documents removed.
   */
  async deleteMany(filter: Filter<S>): Promise<Result<{ deletedCount: number }>> {
    try {
      await this.ensureReady();
      const mapped = mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn);
      const raw = await this._native.deleteMany(this._name, mapped);
      const result = parseRaw<{ deleted: number }>(raw);
      return ok({ deletedCount: result?.deleted ?? 0 });
    } catch (e) {
      return err(toResultError(e));
    }
  }

  /**
   * Counts documents matching `filter`. Defaults to counting all documents when
   * no filter is provided.
   */
  async countDocuments(filter: Filter<S> = {} as Filter<S>): Promise<Result<number>> {
    try {
      await this.ensureReady();
      const mapped = mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn);
      const raw = await this._native.count(this._name, mapped);
      const result = parseRaw<{ count: number }>(raw);
      return ok(result?.count ?? 0);
    } catch (e) {
      return err(toResultError(e));
    }
  }

  /**
   * Returns the unique values of `field` across documents matching `filter`.
   * Defaults to all documents when no filter is provided.
   */
  async distinct(field: string & keyof Document<S>, filter: Filter<S> = {} as Filter<S>): Promise<Result<(string | number | boolean | null)[]>> {
    try {
      await this.ensureReady();
      // Validate field name at runtime — column names can't be parameterized in SQL
      if (!this._knownFields.has(field)) {
        throw new ValidationError({ [field]: { path: field, message: `unknown field: ${field}` } });
      }
      const mapped = mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn);
      const column = this._toColumn(field);
      const raw = await this._native.distinct(this._name, column, mapped);
      const result = parseRaw<(string | number | boolean | null)[]>(raw);
      return ok(result ?? []);
    } catch (e) {
      return err(toResultError(e));
    }
  }

  /**
   * Runs an aggregation pipeline (MongoDB-style) and returns the mapped results.
   * `$group`, `$match`, and accumulator expressions are translated to the native format.
   */
  async aggregate(pipeline: PlainObject[]): Promise<Result<PlainObject[]>> {
    try {
      await this.ensureReady();
      const translated = translateAggregatePipeline(pipeline, this._toColumn) as ZeroshipDbAggregateStage[];
      const raw = await this._native.aggregate(this._name, translated);
      const results = parseRaw<PlainObject[]>(raw);
      return ok((results ?? []).map(d => mapResultDoc(d, this._toField)));
    } catch (e) {
      return err(toResultError(e));
    }
  }
}
