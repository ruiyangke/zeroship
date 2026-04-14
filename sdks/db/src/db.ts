/**
 * createDb — the primary entry point for @zeroship/db.
 *
 * Declares all models upfront, returns a fully-typed db object
 * with collections and transaction support.
 *
 * Usage:
 *   import { createDb } from "@zeroship/db"
 *
 *   const db = createDb({
 *     employees: { name: { type: String, required: true }, email: String },
 *     departments: { name: String, headcount: { type: Number, default: 0 } },
 *   });
 *
 *   // CRUD — { data, error }
 *   const { data } = await db.employees.create({ name: "Alice" });
 *
 *   // Transaction — tx mirrors db, throws on error
 *   const { data, error } = await db.transaction(async (tx) => {
 *     const emp = await tx.employees.create({ name: "Alice" });
 *     await tx.departments.updateOne({ id: 1 }, { headcount: { $inc: 1 } });
 *     return emp;
 *   });
 */

import { model } from "./model.js";
import { Collection, type NativeDb } from "./collection.js";
import { type NormalizedSchema } from "./schema.js";
import { type PlainObject, type Result, ok, err } from "./types.js";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** Schema definition — Mongoose style, builder style, or bare constructors */
type SchemaInput = Record<string, unknown>;

/** A collection inside a transaction — same API but throws on error */
export type TxCollection = {
  create(doc: PlainObject): Promise<PlainObject>;
  insertMany(docs: PlainObject[]): Promise<PlainObject[]>;
  findOne(filter: PlainObject): Promise<PlainObject | null>;
  findById(id: unknown): Promise<PlainObject | null>;
  exists(filter: PlainObject): Promise<boolean>;
  find(filter?: PlainObject): TxQuery;
  updateOne(filter: PlainObject, update: PlainObject): Promise<{ matchedCount: number; modifiedCount: number }>;
  updateMany(filter: PlainObject, update: PlainObject): Promise<{ matchedCount: number; modifiedCount: number }>;
  deleteOne(filter: PlainObject): Promise<{ deletedCount: number }>;
  deleteMany(filter: PlainObject): Promise<{ deletedCount: number }>;
  countDocuments(filter?: PlainObject): Promise<number>;
  distinct(field: string, filter?: PlainObject): Promise<unknown[]>;
  aggregate(pipeline: PlainObject[]): Promise<PlainObject[]>;
};

/** Query inside a transaction — same chainable API but resolves to data directly */
type TxQuery = {
  sort(s: Record<string, number> | string): TxQuery;
  limit(n: number): TxQuery;
  skip(n: number): TxQuery;
  select(s: string | string[] | Record<string, unknown>): TxQuery;
  then(resolve?: (value: PlainObject[]) => unknown, reject?: (reason: unknown) => unknown): Promise<unknown>;
};

/** The db object returned by createDb */
export type Db<T extends Record<string, SchemaInput>> = {
  [K in keyof T]: Collection;
} & {
  transaction: <R>(fn: (tx: { [K in keyof T]: TxCollection }) => Promise<R>) => Promise<Result<R>>;
};

// ---------------------------------------------------------------------------
// TxCollection — wraps a Collection, throws on error
// ---------------------------------------------------------------------------

function createTxCollection(collection: Collection): TxCollection {
  async function unwrap<T>(result: Result<T>): Promise<T> {
    if (result.error) throw result.error;
    return result.data as T;
  }

  return {
    async create(doc: PlainObject) {
      return unwrap(await collection.create(doc));
    },
    async insertMany(docs: PlainObject[]) {
      return unwrap(await collection.insertMany(docs));
    },
    async findOne(filter: PlainObject) {
      return unwrap(await collection.findOne(filter));
    },
    async findById(id: unknown) {
      return unwrap(await collection.findById(id));
    },
    async exists(filter: PlainObject) {
      return unwrap(await collection.exists(filter));
    },
    find(filter: PlainObject = {}): TxQuery {
      const query = collection.find(filter);
      // Wrap the query to throw on error
      return createTxQuery(query);
    },
    async updateOne(filter: PlainObject, update: PlainObject) {
      return unwrap(await collection.updateOne(filter, update));
    },
    async updateMany(filter: PlainObject, update: PlainObject) {
      return unwrap(await collection.updateMany(filter, update));
    },
    async deleteOne(filter: PlainObject) {
      return unwrap(await collection.deleteOne(filter));
    },
    async deleteMany(filter: PlainObject) {
      return unwrap(await collection.deleteMany(filter));
    },
    async countDocuments(filter: PlainObject = {}) {
      return unwrap(await collection.countDocuments(filter));
    },
    async distinct(field: string, filter: PlainObject = {}) {
      return unwrap(await collection.distinct(field, filter));
    },
    async aggregate(pipeline: PlainObject[]) {
      return unwrap(await collection.aggregate(pipeline));
    },
  };
}

/** Wrap a Query to throw on error */
function createTxQuery(query: any): TxQuery {
  return {
    sort(s: Record<string, number>) { query.sort(s); return this; },
    limit(n: number) { query.limit(n); return this; },
    skip(n: number) { query.skip(n); return this; },
    select(s: string | string[]) { query.select(s); return this; },
    then(resolve?: (value: PlainObject[]) => unknown, reject?: (reason: unknown) => unknown) {
      return query.then(
        (result: Result<PlainObject[]>) => {
          if (result.error) throw result.error;
          return resolve ? resolve(result.data as PlainObject[]) : result.data;
        },
        reject
      );
    },
  };
}

// ---------------------------------------------------------------------------
// createDb
// ---------------------------------------------------------------------------

/** Get the native zeroship.db driver */
function getNativeDb(): NativeDb {
  if (typeof globalThis !== "undefined" && (globalThis as any).zeroship?.db) {
    return (globalThis as any).zeroship.db as NativeDb;
  }
  throw new Error("@zeroship/db: native zeroship.db.* not available — are you running inside zeroship?");
}

/**
 * Create a typed database client with all models defined upfront.
 *
 * @param schemas Record of collection names → schema definitions
 * @param nativeOverride Optional: inject a custom native driver (for tests)
 * @returns A db object with typed collections and transaction support
 */
export function createDb<T extends Record<string, SchemaInput>>(
  schemas: T,
  nativeOverride?: NativeDb,
): Db<T> {
  const collections: Record<string, Collection> = {};

  for (const [name, schema] of Object.entries(schemas)) {
    collections[name] = model(name, schema as Record<string, unknown>, nativeOverride);
  }

  const db = {
    ...collections,

    async transaction<R>(fn: (tx: Record<string, TxCollection>) => Promise<R>): Promise<Result<R>> {
      const native = nativeOverride ?? getNativeDb();

      // BEGIN
      await (native as any).beginTransaction();

      // Build tx — same collections but throwing on error
      const tx: Record<string, TxCollection> = {};
      for (const [name, col] of Object.entries(collections)) {
        tx[name] = createTxCollection(col);
      }

      try {
        const result = await fn(tx);
        await (native as any).commitTransaction();
        return ok(result);
      } catch (e) {
        try {
          await (native as any).rollbackTransaction();
        } catch {
          // Ignore rollback errors — connection cleanup handles it
        }
        return err(e instanceof Error ? e : new Error(String(e)));
      }
    },
  };

  return db as Db<T>;
}
