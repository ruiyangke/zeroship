import { NormalizedSchema } from "./schema.js";
import { validateDoc, validatePartial } from "./validate.js";
import { mapNativeError } from "./errors.js";
import {
  mapResultDoc,
  mapFilterOutbound,
  translateAggregatePipeline,
} from "./utils.js";
import { Query } from "./query.js";

type PlainObject = Record<string, unknown>;

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

function parseRaw<T = unknown>(raw: string | null): T | null {
  if (raw === null) return null;
  if (typeof raw === "string") return JSON.parse(raw) as T;
  return raw as unknown as T;
}

// Extract plain field object from an update argument.
// Handles both { $set: { field: val } } and { field: val } styles.
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

export class Collection {
  private _name: string;
  private _schema: NormalizedSchema;
  private _native: NativeDb;

  constructor(name: string, schema: NormalizedSchema, native: NativeDb) {
    this._name = name;
    this._schema = schema;
    this._native = native;
  }

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

  find(filter: PlainObject = {}): Query {
    const mapped = mapFilterOutbound(filter);
    return new Query(
      this._name,
      mapped,
      (col, f, opts) => this._native.find(col, f, opts)
    );
  }

  async updateOne(
    filter: PlainObject,
    update: PlainObject
  ): Promise<{ matchedCount: number; modifiedCount: number }> {
    const fields = extractUpdateFields(update);
    validatePartial(fields, this._schema);
    const mappedFilter = mapFilterOutbound(filter);
    try {
      const raw = await this._native.updateOne(
        this._name,
        mappedFilter,
        update
      );
      const result = parseRaw(raw);
      const matched = result !== null ? 1 : 0;
      return { matchedCount: matched, modifiedCount: matched };
    } catch (err) {
      throw mapNativeError(String(err instanceof Error ? err.message : err));
    }
  }

  async updateMany(
    filter: PlainObject,
    update: PlainObject
  ): Promise<{ matchedCount: number; modifiedCount: number }> {
    const fields = extractUpdateFields(update);
    validatePartial(fields, this._schema);
    const mappedFilter = mapFilterOutbound(filter);
    try {
      const raw = await this._native.updateMany(
        this._name,
        mappedFilter,
        update
      );
      const result = parseRaw<{ updated: number }>(raw);
      const n = result?.updated ?? 0;
      return { matchedCount: n, modifiedCount: n };
    } catch (err) {
      throw mapNativeError(String(err instanceof Error ? err.message : err));
    }
  }

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
