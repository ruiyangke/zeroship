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
import { PlainObject, Result, Document, CreateInput, UpdateInput, ok, err } from "./types.js";

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
function parseRaw<T = unknown>(raw: string | null | undefined): T | null {
  if (raw === null || raw === undefined) return null;
  if (typeof raw === "string") {
    if (raw === "") return null;
    try {
      return JSON.parse(raw) as T;
    } catch (e: unknown) {
      throw new Error(`failed to parse native response: ${e instanceof Error ? e.message : String(e)}`, { cause: e });
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
 * The generic parameter `S` is the raw schema shape from which document and input
 * types are derived. Use `model()` or `createDb()` — do not construct directly.
 */
export class Collection<S = PlainObject> {
  private _name: string;
  private _schema: NormalizedSchema;
  private _native: NativeDb;
  private _ready: Promise<unknown> | null;

  constructor(name: string, schema: NormalizedSchema, native: NativeDb, registrationPromise?: Promise<unknown> | null) {
    this._name = name;
    this._schema = schema;
    this._native = native;
    this._ready = registrationPromise ?? null;
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
      const raw = await this._native.insert(this._name, validated);
      const result = parseRaw<PlainObject>(raw);
      return ok(mapResultDoc(result!) as Document<S>);
    } catch (e) {
      return err(toResultError(e));
    }
  }

  /**
   * Inserts multiple documents after validating each one against the schema.
   * Returns the persisted documents with field names mapped to the user-facing shape.
   */
  async insertMany(docs: CreateInput<S>[]): Promise<Result<Document<S>[]>> {
    await this.ensureReady();
      try {
      const validated = (docs as PlainObject[]).map((doc) => validateDoc(doc, this._schema));
      const raw = await this._native.insertMany(this._name, validated);
      const results = parseRaw<PlainObject[]>(raw);
      return ok((results ?? []).map(mapResultDoc) as Document<S>[]);
    } catch (e) {
      return err(toResultError(e));
    }
  }

  /**
   * Finds and returns the first document matching `filter`, or `null` if none exists.
   * Field names in `filter` are mapped outbound before the native call.
   */
  async findOne(filter: Partial<Document<S>>): Promise<Result<Document<S> | null>> {
    await this.ensureReady();
      try {
      const mapped = mapFilterOutbound(filter as PlainObject);
      const raw = await this._native.findOne(this._name, mapped);
      if (raw === null) return ok(null);
      const result = parseRaw<PlainObject>(raw);
      if (result === null) return ok(null);
      return ok(mapResultDoc(result) as Document<S>);
    } catch (e) {
      return err(toResultError(e));
    }
  }

  /** Shorthand for `findOne({ id })`. */
  async findById(id: unknown): Promise<Result<Document<S> | null>> {
    return this.findOne({ id } as Partial<Document<S>>);
  }

  /** Returns true if at least one document matches `filter`. */
  async exists(filter: Partial<Document<S>>): Promise<Result<boolean>> {
    const { data, error } = await this.countDocuments(filter);
    if (error) return err(error);
    return ok((data ?? 0) > 0);
  }

  /**
   * Returns a lazy Query that can be chained with `.sort()`, `.limit()`, `.skip()`,
   * and `.select()` before being awaited.
   */
  find(filter: Partial<Document<S>> = {} as Partial<Document<S>>): Query<S> {
    const mapped = mapFilterOutbound(filter as PlainObject);
    const ready = this._ready;
    return new Query<S>(
      this._name,
      mapped,
      async (col, f, opts) => {
        if (ready) await ready;
        return this._native.find(col, f, opts);
      }
    );
  }

  /**
   * Updates the first document matching `filter` using the given `update`.
   * Validates the fields in `$set` and bare (non-`$`) keys against the schema.
   * Validates `$push`/`$addToSet` values against the declared array item type.
   * Returns `{ matchedCount, modifiedCount }` indicating whether a document was found.
   */
  async updateOne(
    filter: Partial<Document<S>>,
    update: UpdateInput<S> | PlainObject
  ): Promise<Result<{ matchedCount: number; modifiedCount: number }>> {
    await this.ensureReady();
      try {
      const updateObj = update as PlainObject;
      const fields = extractUpdateFields(updateObj);
      validatePartial(fields, this._schema);
      validateArrayPushOps(updateObj, this._schema);
      const mappedFilter = mapFilterOutbound(filter as PlainObject);
      const mappedUpdate = mapUpdateOutbound(updateObj);
      const raw = await this._native.updateOne(
        this._name,
        mappedFilter,
        mappedUpdate
      );
      const result = parseRaw(raw);
      const matched = result !== null && typeof result === "object" ? 1 : 0;
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
    filter: Partial<Document<S>>,
    update: UpdateInput<S> | PlainObject
  ): Promise<Result<{ matchedCount: number; modifiedCount: number }>> {
    await this.ensureReady();
      try {
      const updateObj = update as PlainObject;
      const fields = extractUpdateFields(updateObj);
      validatePartial(fields, this._schema);
      validateArrayPushOps(updateObj, this._schema);
      const mappedFilter = mapFilterOutbound(filter as PlainObject);
      const mappedUpdate = mapUpdateOutbound(updateObj);
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
  async deleteOne(filter: Partial<Document<S>>): Promise<Result<{ deletedCount: number }>> {
    await this.ensureReady();
      try {
      const mapped = mapFilterOutbound(filter as PlainObject);
      const raw = await this._native.deleteOne(this._name, mapped);
      const result = parseRaw(raw);
      return ok({ deletedCount: result !== null ? 1 : 0 });
    } catch (e) {
      return err(toResultError(e));
    }
  }

  /**
   * Deletes all documents matching `filter`.
   * Returns `{ deletedCount: N }` where N is the number of documents removed.
   */
  async deleteMany(filter: Partial<Document<S>>): Promise<Result<{ deletedCount: number }>> {
    await this.ensureReady();
      try {
      const mapped = mapFilterOutbound(filter as PlainObject);
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
  async countDocuments(filter: Partial<Document<S>> = {} as Partial<Document<S>>): Promise<Result<number>> {
    await this.ensureReady();
      try {
      const mapped = mapFilterOutbound(filter as PlainObject);
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
  async distinct(field: string, filter: Partial<Document<S>> = {} as Partial<Document<S>>): Promise<Result<unknown[]>> {
    await this.ensureReady();
      try {
      const mapped = mapFilterOutbound(filter as PlainObject);
      const raw = await this._native.distinct(this._name, field, mapped);
      const result = parseRaw<unknown[]>(raw);
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
    await this.ensureReady();
      try {
      const translated = translateAggregatePipeline(pipeline) as ZeroshipDbAggregateStage[];
      const raw = await this._native.aggregate(this._name, translated);
      const results = parseRaw<PlainObject[]>(raw);
      return ok((results ?? []).map(mapResultDoc));
    } catch (e) {
      return err(toResultError(e));
    }
  }
}
