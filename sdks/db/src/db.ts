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
import { Query } from "./query.js";
import { type NormalizedSchema } from "./schema.js";
import { type PlainObject, type Result, type Document, type CreateInput, type UpdateExpression, type Filter, type IsolationLevel, type NamingStrategy, naming, ok, err } from "./types.js";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** Schema definition — Mongoose style, builder style, or bare constructors */
type SchemaInput = Record<string, unknown>;

/**
 * A typed collection inside a transaction — same API as Collection but throws
 * on error instead of returning Result. Generic over schema shape S.
 */
export type TxCollection<S = PlainObject> = {
  create(doc: CreateInput<S>): Promise<Document<S>>;
  insertMany(docs: CreateInput<S>[]): Promise<Document<S>[]>;
  findOne(filter: Filter<S>): Promise<Document<S> | null>;
  findById(id: number): Promise<Document<S> | null>;
  exists(filter: Filter<S>): Promise<boolean>;
  find(filter?: Filter<S>): TxQuery<S, Document<S>>;
  upsert(doc: CreateInput<S>, options: { conflictFields: (string & keyof Document<S>)[] }): Promise<Document<S>>;
  updateOne(filter: Filter<S>, update: UpdateExpression<S>): Promise<{ matchedCount: number; modifiedCount: number }>;
  updateMany(filter: Filter<S>, update: UpdateExpression<S>): Promise<{ matchedCount: number; modifiedCount: number }>;
  findOneAndUpdate(filter: Filter<S>, update: UpdateExpression<S>): Promise<Document<S> | null>;
  findOneAndDelete(filter: Filter<S>): Promise<Document<S> | null>;
  deleteOne(filter: Filter<S>): Promise<{ deletedCount: number }>;
  deleteMany(filter: Filter<S>): Promise<{ deletedCount: number }>;
  forceDelete(filter: Filter<S>): Promise<{ deletedCount: number }>;
  forceDeleteMany(filter: Filter<S>): Promise<{ deletedCount: number }>;
  countDocuments(filter?: Filter<S>): Promise<number>;
  distinct(field: string & keyof Document<S>, filter?: Filter<S>): Promise<(string | number | boolean | null)[]>;
  aggregate(pipeline: PlainObject[]): Promise<PlainObject[]>;
};

/** Query inside a transaction — same chainable API but resolves to data directly */
export type TxQuery<S = PlainObject, P = Document<S>> = {
  sort(s: Record<string, number> | string): TxQuery<S, P>;
  limit(n: number): TxQuery<S, P>;
  skip(n: number): TxQuery<S, P>;
  select<K extends keyof Document<S> & string>(fields: K[]): TxQuery<S, Pick<Document<S>, K>>;
  select(s: string | string[] | Record<string, number | boolean>): TxQuery<S, P>;
  after(id: number): TxQuery<S, P>;
  then<TResult1 = P[], TResult2 = never>(
    resolve?: ((value: P[]) => TResult1 | PromiseLike<TResult1>) | null,
    reject?: ((reason: unknown) => TResult2 | PromiseLike<TResult2>) | null
  ): Promise<TResult1 | TResult2>;
};

/** Options for the transaction method. */
export interface TransactionOptions {
  /** PostgreSQL transaction isolation level. Defaults to the database default (read committed). */
  isolationLevel?: IsolationLevel;
}

/** The db object returned by createDb — collections are fully typed per schema */
export type Db<T extends Record<string, SchemaInput>> = {
  [K in keyof T]: Collection<T[K]>
} & {
  transaction: <R>(fn: (tx: { [K in keyof T]: TxCollection<T[K]> }) => Promise<R>, options?: TransactionOptions) => Promise<Result<R>>;
};

// ---------------------------------------------------------------------------
// TxCollection — wraps a Collection, throws on error
// ---------------------------------------------------------------------------

/** Extracts data from Result, throws on error. Shared across all TxCollections. */
async function unwrap<T>(result: Result<T>): Promise<T> {
  if (result.error) throw result.error;
  return result.data as T;
}

function createTxCollection<S>(collection: Collection<S>): TxCollection<S> {

  return {
    async create(doc: CreateInput<S>) {
      return unwrap(await collection.create(doc));
    },
    async insertMany(docs: CreateInput<S>[]) {
      return unwrap(await collection.insertMany(docs));
    },
    async findOne(filter: Filter<S>) {
      return unwrap(await collection.findOne(filter));
    },
    async findById(id: number) {
      return unwrap(await collection.findById(id));
    },
    async exists(filter: Filter<S>) {
      return unwrap(await collection.exists(filter));
    },
    find(filter: Filter<S> = {} as Filter<S>): TxQuery<S, Document<S>> {
      const query = collection.find(filter);
      return createTxQuery<S>(query);
    },
    async upsert(doc: CreateInput<S>, options: { conflictFields: (string & keyof Document<S>)[] }) {
      return unwrap(await collection.upsert(doc, options));
    },
    async updateOne(filter: Filter<S>, update: UpdateExpression<S>) {
      return unwrap(await collection.updateOne(filter, update));
    },
    async updateMany(filter: Filter<S>, update: UpdateExpression<S>) {
      return unwrap(await collection.updateMany(filter, update));
    },
    async findOneAndUpdate(filter: Filter<S>, update: UpdateExpression<S>) {
      return unwrap(await collection.findOneAndUpdate(filter, update));
    },
    async findOneAndDelete(filter: Filter<S>) {
      return unwrap(await collection.findOneAndDelete(filter));
    },
    async deleteOne(filter: Filter<S>) {
      return unwrap(await collection.deleteOne(filter));
    },
    async deleteMany(filter: Filter<S>) {
      return unwrap(await collection.deleteMany(filter));
    },
    async forceDelete(filter: Filter<S>) {
      return unwrap(await collection.forceDelete(filter));
    },
    async forceDeleteMany(filter: Filter<S>) {
      return unwrap(await collection.forceDeleteMany(filter));
    },
    async countDocuments(filter: Filter<S> = {} as Filter<S>) {
      return unwrap(await collection.countDocuments(filter));
    },
    async distinct(field: string & keyof Document<S>, filter: Filter<S> = {} as Filter<S>) {
      return unwrap(await collection.distinct(field, filter));
    },
    async aggregate(pipeline: PlainObject[]) {
      return unwrap(await collection.aggregate(pipeline));
    },
  };
}

/** Wrap a Query to throw on error */
function createTxQuery<S>(query: Query<S, Document<S>>): TxQuery<S, Document<S>> {
  return {
    sort(s: Record<string, number> | string) { query.sort(s); return this; },
    limit(n: number) { query.limit(n); return this; },
    skip(n: number) { query.skip(n); return this; },
    select(s: string | string[] | Record<string, number | boolean>) { query.select(s as any); return this as any; },
    after(id: number) { query.after(id); return this; },
    then(resolve?: ((value: Document<S>[]) => any) | null, reject?: ((reason: unknown) => any) | null) {
      return query.then(
        (result: Result<Document<S>[]>) => {
          if (result.error) throw result.error;
          return resolve ? resolve(result.data as Document<S>[]) : result.data;
        },
        reject
      ) as any;
    },
  };
}

// ---------------------------------------------------------------------------
// createDb
// ---------------------------------------------------------------------------

/** Get the native zeroship.db driver */
function getNativeDb(): NativeDb {
  if (typeof zeroship !== "undefined" && zeroship?.db) {
    return zeroship.db;
  }
  throw new Error("@zeroship/db: native zeroship.db.* not available — are you running inside zeroship?");
}

/** Options for createDb. */
export interface CreateDbOptions {
  /** Custom native driver (for tests). */
  native?: NativeDb;
  /** Column naming strategy. Default: `naming.snakeCase`. */
  naming?: NamingStrategy;
  /** Enable soft delete for all collections. When true, deleteOne/deleteMany set `deleted_at` instead of removing rows, and all reads auto-filter deleted documents. */
  softDelete?: boolean;
}

/**
 * Create a typed database client with all models defined upfront.
 *
 * @param schemas Record of collection names → schema definitions
 * @param options Optional: native driver override and naming strategy
 * @returns A db object with typed collections and transaction support
 */
export function createDb<const T extends Record<string, SchemaInput>>(
  schemas: T,
  options?: CreateDbOptions,
): Db<T> {
  const native = options?.native ?? getNativeDb();
  const namingStrategy = options?.naming ?? naming.snakeCase;
  const softDelete = options?.softDelete ?? false;
  const collections = {} as { [K in keyof T]: Collection<T[K]> };

  for (const [name, schema] of Object.entries(schemas)) {
    (collections as Record<string, Collection<SchemaInput>>)[name] =
      model(name, schema as SchemaInput, native, namingStrategy, softDelete);
  }

  // Pre-cache TxCollection wrappers — stateless, reusable across transactions
  const txCollections = {} as { [K in keyof T]: TxCollection<T[K]> };
  for (const [name, col] of Object.entries(collections)) {
    (txCollections as Record<string, TxCollection<SchemaInput>>)[name] =
      createTxCollection(col as Collection<SchemaInput>);
  }

  const db = {
    ...collections,

    async transaction<R>(fn: (tx: { [K in keyof T]: TxCollection<T[K]> }) => Promise<R>, options?: TransactionOptions): Promise<Result<R>> {
      // Detect native error envelope: Rust resolves (not rejects) with {"error":"..."}
      function checkTxResult(raw: unknown): void {
        if (raw && typeof raw === "string") {
          try {
            const parsed = JSON.parse(raw);
            if (parsed && typeof parsed === "object" && typeof parsed.error === "string") {
              throw new Error(parsed.error);
            }
          } catch (e) {
            if (e instanceof Error && !e.message.startsWith("Unexpected")) throw e;
          }
        }
      }

      // BEGIN
      const beginResult = await native.beginTransaction?.(options?.isolationLevel);
      checkTxResult(beginResult);

      try {
        const result = await fn(txCollections);
        const commitResult = await native.commitTransaction?.();
        checkTxResult(commitResult);
        return ok(result);
      } catch (e) {
        try {
          await native.rollbackTransaction?.();
        } catch {
          // Ignore rollback errors — connection cleanup handles it
        }
        return err(e instanceof Error ? e : new Error(String(e)));
      }
    },
  };

  return db as Db<T>;
}
