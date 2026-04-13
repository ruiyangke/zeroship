/**
 * Collection: the main entry point for CRUD operations on a named collection.
 * Each method validates inputs against the schema, maps field names to the native
 * format, calls the native driver, and maps results back to the user-facing shape.
 */
import { NormalizedSchema } from "./schema.js";
import { validateDoc, validatePartial } from "./validate.js";
import { mapNativeError, ValidationError } from "./errors.js";
import {
  mapResultDoc,
  mapFilterOutbound,
  mapUpdateOutbound,
  translateAggregatePipeline,
} from "./utils.js";
import { Query } from "./query.js";
import { PlainObject } from "./types.js";

/** Interface that the native appbase.db.* layer must satisfy. */
export interface NativeDb {
  insert(collection: string, doc: unknown): Promise<string>;
  insertMany(collection: string, docs: unknown): Promise<string>;
  findOne(collection: string, filter: unknown): Promise<string | null>;
  find(collection: string, filter: unknown, opts: unknown): Promise<string>;
  updateOne(
    collection: string,
    filter: unknown,
    update: unknown
  ): Promise<string>;
  updateMany(
    collection: string,
    filter: unknown,
    update: unknown
  ): Promise<string>;
  deleteOne(collection: string, filter: unknown): Promise<string>;
  deleteMany(collection: string, filter: unknown): Promise<string>;
  count(collection: string, filter: unknown): Promise<string>;
  distinct(
    collection: string,
    field: string,
    filter: unknown
  ): Promise<string>;
  aggregate(collection: string, pipeline: unknown): Promise<string>;
}

/** Parses a raw JSON string (or already-parsed value) from the native layer. */
function parseRaw<T = unknown>(raw: string | null | undefined): T | null {
  if (raw === null || raw === undefined) return null;
  if (typeof raw === "string") {
    if (raw === "") return null;
    try {
      return JSON.parse(raw) as T;
    } catch (e: any) {
      throw new Error(`failed to parse native response: ${e.message ?? e}`, { cause: e });
    }
  }
  return raw as unknown as T;
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
      Object.assign(fields, val as PlainObject);
    } else if (!key.startsWith("$")) {
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
 * Instances are created via `model()` — do not construct directly in application code.
 */
export class Collection {
  private _name: string;
  private _schema: NormalizedSchema;
  private _native: NativeDb;

  constructor(name: string, schema: NormalizedSchema, native: NativeDb) {
    this._name = name;
    this._schema = schema;
    this._native = native;
  }

  /**
   * Inserts a single document after validating it against the schema.
   * Returns the persisted document with `_id`, `createdAt`, and `updatedAt` mapped.
   */
  async create(doc: PlainObject): Promise<PlainObject> {
    const validated = validateDoc(doc, this._schema);
    try {
      const raw = await this._native.insert(this._name, validated);
      const result = parseRaw<PlainObject>(raw);
      return mapResultDoc(result!);
    } catch (err) {
      throw mapNativeError(String(err instanceof Error ? err.message : err));
    }
  }

  /**
   * Alias for `create()`. Inserts a single document after validating it against the schema.
   * Provided for spec compatibility — both `insert()` and `create()` are supported.
   */
  async insert(doc: PlainObject): Promise<PlainObject> {
    return this.create(doc);
  }

  /**
   * Inserts multiple documents after validating each one against the schema.
   * Returns the persisted documents with field names mapped to the user-facing shape.
   */
  async insertMany(docs: PlainObject[]): Promise<PlainObject[]> {
    const validated = docs.map((doc) => validateDoc(doc, this._schema));
    try {
      const raw = await this._native.insertMany(this._name, validated);
      const results = parseRaw<PlainObject[]>(raw);
      return (results ?? []).map(mapResultDoc);
    } catch (err) {
      throw mapNativeError(String(err instanceof Error ? err.message : err));
    }
  }

  /**
   * Finds and returns the first document matching `filter`, or `null` if none exists.
   * Field names in `filter` are mapped outbound before the native call.
   */
  async findOne(filter: PlainObject): Promise<PlainObject | null> {
    const mapped = mapFilterOutbound(filter);
    try {
      const raw = await this._native.findOne(this._name, mapped);
      if (raw === null) return null;
      const result = parseRaw<PlainObject>(raw);
      if (result === null) return null;
      return mapResultDoc(result);
    } catch (err) {
      throw mapNativeError(String(err instanceof Error ? err.message : err));
    }
  }

  /**
   * Returns a lazy Query that can be chained with `.sort()`, `.limit()`, `.skip()`,
   * and `.select()` before being awaited.
   */
  find(filter: PlainObject = {}): Query {
    const mapped = mapFilterOutbound(filter);
    return new Query(
      this._name,
      mapped,
      (col, f, opts) => this._native.find(col, f, opts)
    );
  }

  /**
   * Updates the first document matching `filter` using the given `update`.
   * Validates the fields in `$set` and bare (non-`$`) keys against the schema.
   * Validates `$push`/`$addToSet` values against the declared array item type.
   * Returns `{ matchedCount, modifiedCount }` indicating whether a document was found.
   */
  async updateOne(
    filter: PlainObject,
    update: PlainObject
  ): Promise<{ matchedCount: number; modifiedCount: number }> {
    const fields = extractUpdateFields(update);
    validatePartial(fields, this._schema);
    validateArrayPushOps(update, this._schema);
    const mappedFilter = mapFilterOutbound(filter);
    const mappedUpdate = mapUpdateOutbound(update);
    try {
      const raw = await this._native.updateOne(
        this._name,
        mappedFilter,
        mappedUpdate
      );
      const result = parseRaw(raw);
      const matched = result !== null && typeof result === "object" ? 1 : 0;
      return { matchedCount: matched, modifiedCount: matched };
    } catch (err) {
      throw mapNativeError(String(err instanceof Error ? err.message : err));
    }
  }

  /**
   * Updates all documents matching `filter` using the given `update`.
   * Validates the fields in `$set` and bare keys against the schema.
   * Validates `$push`/`$addToSet` values against the declared array item type.
   * Returns `{ matchedCount, modifiedCount }` with the count from the native layer.
   */
  async updateMany(
    filter: PlainObject,
    update: PlainObject
  ): Promise<{ matchedCount: number; modifiedCount: number }> {
    const fields = extractUpdateFields(update);
    validatePartial(fields, this._schema);
    validateArrayPushOps(update, this._schema);
    const mappedFilter = mapFilterOutbound(filter);
    const mappedUpdate = mapUpdateOutbound(update);
    try {
      const raw = await this._native.updateMany(
        this._name,
        mappedFilter,
        mappedUpdate
      );
      const result = parseRaw<{ updated: number }>(raw);
      const n = result?.updated ?? 0;
      return { matchedCount: n, modifiedCount: n };
    } catch (err) {
      throw mapNativeError(String(err instanceof Error ? err.message : err));
    }
  }

  /**
   * Deletes the first document matching `filter`.
   * Returns `{ deletedCount: 1 }` if a document was found, `{ deletedCount: 0 }` otherwise.
   */
  async deleteOne(filter: PlainObject): Promise<{ deletedCount: number }> {
    const mapped = mapFilterOutbound(filter);
    try {
      const raw = await this._native.deleteOne(this._name, mapped);
      const result = parseRaw(raw);
      return { deletedCount: result !== null ? 1 : 0 };
    } catch (err) {
      throw mapNativeError(String(err instanceof Error ? err.message : err));
    }
  }

  /**
   * Deletes all documents matching `filter`.
   * Returns `{ deletedCount: N }` where N is the number of documents removed.
   */
  async deleteMany(filter: PlainObject): Promise<{ deletedCount: number }> {
    const mapped = mapFilterOutbound(filter);
    try {
      const raw = await this._native.deleteMany(this._name, mapped);
      const result = parseRaw<{ deleted: number }>(raw);
      return { deletedCount: result?.deleted ?? 0 };
    } catch (err) {
      throw mapNativeError(String(err instanceof Error ? err.message : err));
    }
  }

  /**
   * Counts documents matching `filter`. Defaults to counting all documents when
   * no filter is provided.
   */
  async countDocuments(filter: PlainObject = {}): Promise<number> {
    const mapped = mapFilterOutbound(filter);
    try {
      const raw = await this._native.count(this._name, mapped);
      const result = parseRaw<{ count: number }>(raw);
      return result?.count ?? 0;
    } catch (err) {
      throw mapNativeError(String(err instanceof Error ? err.message : err));
    }
  }

  /**
   * Returns the unique values of `field` across documents matching `filter`.
   * Defaults to all documents when no filter is provided.
   */
  async distinct(field: string, filter: PlainObject = {}): Promise<unknown[]> {
    const mapped = mapFilterOutbound(filter);
    try {
      const raw = await this._native.distinct(this._name, field, mapped);
      const result = parseRaw<unknown[]>(raw);
      return result ?? [];
    } catch (err) {
      throw mapNativeError(String(err instanceof Error ? err.message : err));
    }
  }

  /**
   * Runs an aggregation pipeline (MongoDB-style) and returns the mapped results.
   * `$group`, `$match`, and accumulator expressions are translated to the native format.
   */
  async aggregate(pipeline: PlainObject[]): Promise<PlainObject[]> {
    const translated = translateAggregatePipeline(pipeline);
    try {
      const raw = await this._native.aggregate(this._name, translated);
      const results = parseRaw<PlainObject[]>(raw);
      return (results ?? []).map(mapResultDoc);
    } catch (err) {
      throw mapNativeError(String(err instanceof Error ? err.message : err));
    }
  }
}
