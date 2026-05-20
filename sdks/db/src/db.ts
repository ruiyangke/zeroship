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
 *   const { data } = await db.employees.insert({ name: "Alice" });
 *
 *   // Transaction — tx mirrors db, throws on error
 *   const { data, error } = await db.transaction(async (tx) => {
 *     const emp = await tx.employees.insert({ name: "Alice" });
 *     await tx.departments.update(1, { headcount: { $inc: 1 } });
 *     return emp;
 *   });
 */

import { env } from "zeroship";
import { model } from "./model.js";
import { Collection, type NativeDb } from "./collection.js";
import { Query } from "./query.js";
import { type NormalizedSchema, normalizeSchema, validateRefTargets } from "./schema.js";
import { type PlainObject, type Result, type Row, type RowInput, type UpdateExpression, type Filter, type IsolationLevel, type NamingStrategy, SchemaBuilder, TypeBuilder, naming, ok, err } from "./types.js";

/**
 * Topologically sort schema names so parents precede children. A child
 * is a collection with `t.ref(parent)` somewhere in its field set.
 * Used by `createDb` to chain `registerModel` calls in dependency
 * order. Cycles (mutual refs) fall back to declaration order — they're
 * resolved by `DEFERRABLE INITIALLY DEFERRED` at the SQL layer.
 */
function topoSortByRefs(schemas: Record<string, unknown>): string[] {
  const names = Object.keys(schemas);
  const deps = new Map<string, Set<string>>();
  for (const name of names) {
    deps.set(name, new Set());
    const raw = schemas[name];
    const fields = (raw instanceof SchemaBuilder ? raw.fields : raw) as Record<string, unknown> | unknown;
    if (!fields || typeof fields !== "object") continue;
    for (const fd of Object.values(fields as Record<string, unknown>)) {
      const def = fd instanceof TypeBuilder ? fd.toFieldDef() : (fd as { type?: string; refTarget?: string });
      if (def && (def as { type?: string }).type === "ref") {
        const target = (def as { refTarget?: string }).refTarget;
        if (target && target !== name && names.includes(target)) {
          deps.get(name)!.add(target);
        }
      }
    }
  }
  const visited = new Set<string>();
  const onStack = new Set<string>();
  const out: string[] = [];
  function visit(n: string): void {
    if (visited.has(n)) return;
    if (onStack.has(n)) return; // cycle — break; DEFERRABLE handles it
    onStack.add(n);
    for (const d of deps.get(n)!) visit(d);
    onStack.delete(n);
    visited.add(n);
    out.push(n);
  }
  for (const n of names) visit(n);
  return out;
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/**
 * Schema definition — plain fields, schema() builder with options, or
 * a top-level `t.union(...)` whose row shape is a discriminated union
 * (proposal §C2). The TypeBuilder form is type-erased to
 * `TypeBuilder<unknown, any>` here so the conditional in `UnwrapSchema`
 * can distribute over the union.
 */
type SchemaInput =
  | Record<string, unknown>
  | SchemaBuilder<Record<string, unknown>>
  | TypeBuilder<unknown, any>;

/**
 * A typed collection inside a transaction — same API as Collection but throws
 * on error instead of returning Result. Generic over schema shape S.
 */
export type TxCollection<S = PlainObject> = {
  insert(row: RowInput<S>): Promise<Row<S>>;
  insertMany(rows: RowInput<S>[]): Promise<Row<S>[]>;
  get(
    idOrFilter: number | Filter<S>,
    opts?: { select?: (string & keyof Row<S>)[]; orderBy?: Record<string, 1 | -1> },
  ): Promise<Row<S> | null>;
  exists(filter: Filter<S>): Promise<boolean>;
  find(filter?: Filter<S>): TxQuery<S, Row<S>>;
  upsert(row: RowInput<S>, options: { conflictFields: (string & keyof Row<S>)[] }): Promise<Row<S>>;
  update(idOrFilter: number | Filter<S>, patch: UpdateExpression<S>): Promise<Row<S> | null>;
  updateMany(filter: Filter<S>, patch: UpdateExpression<S>): Promise<{ matchedCount: number; modifiedCount: number }>;
  delete(idOrFilter: number | Filter<S>, opts?: { hard?: boolean }): Promise<Row<S> | null>;
  deleteMany(filter: Filter<S>, opts?: { hard?: boolean }): Promise<{ deletedCount: number }>;
  count(filter?: Filter<S>): Promise<number>;
  distinct(field: string & keyof Row<S>, filter?: Filter<S>): Promise<(string | number | boolean | null)[]>;
  aggregate(pipeline: ZeroshipDbAggregateStage[]): Promise<PlainObject[]>;
};

/** Query inside a transaction — same chainable API but resolves to data directly */
export type TxQuery<S = PlainObject, P = Row<S>> = {
  sort(s: Record<string, number> | string): TxQuery<S, P>;
  limit(n: number): TxQuery<S, P>;
  skip(n: number): TxQuery<S, P>;
  select<K extends keyof Row<S> & string>(fields: K[]): TxQuery<S, Pick<Row<S>, K>>;
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

/**
 * Unwrap SchemaBuilder / TypeBuilder at the type level so the collection
 * receives the underlying field record (or, for a top-level
 * `t.union(...)`, the inferred union shape — see proposal §C2).
 *
 * - `schema({...})` wraps a `Record<string, unknown>` and we strip it.
 * - `t.union(...)` produces `TypeBuilder<UnionShape>`; we extract
 *   `UnionShape` so the collection is `Collection<UnionShape>` and a
 *   `find()` returns `Row<UnionShape>` whose discriminator key
 *   narrows correctly under control flow analysis.
 */
type UnwrapSchema<T> =
  T extends SchemaBuilder<infer S> ? S :
  T extends TypeBuilder<infer U, any> ? U :
  T;

/** The db object returned by createDb — collections are fully typed per schema */
export type Db<T extends Record<string, SchemaInput>> = {
  [K in keyof T]: Collection<UnwrapSchema<T[K]>>
} & {
  transaction: <R>(fn: (tx: { [K in keyof T]: TxCollection<UnwrapSchema<T[K]>> }) => Promise<R>, options?: TransactionOptions) => Promise<Result<R>>;
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

  const tx: TxCollection<S> = {
    async insert(row: RowInput<S>) {
      return unwrap(await collection.insert(row));
    },
    async insertMany(rows: RowInput<S>[]) {
      return unwrap(await collection.insertMany(rows));
    },
    async get(
      idOrFilter: number | Filter<S>,
      opts?: { select?: (string & keyof Row<S>)[]; orderBy?: Record<string, 1 | -1> },
    ) {
      return unwrap(await collection.get(idOrFilter, opts));
    },
    async exists(filter: Filter<S>) {
      return unwrap(await collection.exists(filter));
    },
    find(filter: Filter<S> = {} as Filter<S>): TxQuery<S, Row<S>> {
      const query = collection.find(filter);
      return createTxQuery<S>(query);
    },
    async upsert(row: RowInput<S>, options: { conflictFields: (string & keyof Row<S>)[] }) {
      return unwrap(await collection.upsert(row, options));
    },
    async update(idOrFilter: number | Filter<S>, patch: UpdateExpression<S>) {
      return unwrap(await collection.update(idOrFilter, patch));
    },
    async updateMany(filter: Filter<S>, patch: UpdateExpression<S>) {
      return unwrap(await collection.updateMany(filter, patch));
    },
    async delete(idOrFilter: number | Filter<S>, opts?: { hard?: boolean }) {
      return unwrap(await collection.delete(idOrFilter, opts));
    },
    async deleteMany(filter: Filter<S>, opts?: { hard?: boolean }) {
      return unwrap(await collection.deleteMany(filter, opts));
    },
    async count(filter: Filter<S> = {} as Filter<S>) {
      return unwrap(await collection.count(filter));
    },
    async distinct(field: string & keyof Row<S>, filter: Filter<S> = {} as Filter<S>) {
      return unwrap(await collection.distinct(field, filter));
    },
    async aggregate(pipeline: ZeroshipDbAggregateStage[]) {
      return unwrap(await collection.aggregate(pipeline));
    },
  };
  return tx;
}

/** Wrap a Query to throw on error */
function createTxQuery<S>(query: Query<S, Row<S>>): TxQuery<S, Row<S>> {
  return {
    sort(s: Record<string, number> | string) { query.sort(s); return this; },
    limit(n: number) { query.limit(n); return this; },
    skip(n: number) { query.skip(n); return this; },
    select(s: string | string[] | Record<string, number | boolean>) { query.select(s as any); return this as any; },
    after(id: number) { query.after(id); return this; },
    then(resolve?: ((value: Row<S>[]) => any) | null, reject?: ((reason: unknown) => any) | null) {
      return query.then(
        (result: Result<Row<S>[]>) => {
          if (result.error) throw result.error;
          return resolve ? resolve(result.data as Row<S>[]) : result.data;
        },
        reject
      ) as any;
    },
  };
}

// ---------------------------------------------------------------------------
// createDb
// ---------------------------------------------------------------------------

/**
 * Resolve the native database driver off the runtime's composite `env`.
 *
 * Each V8 isolate in the zeroship runtime builds an `env` object at init
 * time that overlays plugin namespaces on top of the app's scalar env
 * vars (see `crates/runtime/src/plugin.rs::build_env_object`). The
 * DbPlugin registers under namespace "db", so `env.db` is the live
 * NativeDb interface with `find`, `findOne`, `insert`, ... attached.
 *
 * The same `env` object is also passed as the second argument to
 * `fetch(request, env, ctx)`. SDK users who prefer to receive it
 * explicitly can pass it via `createDb(..., { native: env.db })`.
 */
function getNativeDb(): NativeDb {
  const db = (env as { db?: NativeDb } | undefined)?.db;
  if (db) {
    return db;
  }
  throw new Error(
    "@zeroship/db: env.db not available — " +
    "is the DbPlugin registered on this runtime?"
  );
}

/** Options for createDb. */
export interface CreateDbOptions {
  /** Custom native driver (for tests). */
  native?: NativeDb;
  /** Column naming strategy. Default: `naming.snakeCase`. */
  naming?: NamingStrategy;
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
  const collections = {} as { [K in keyof T]: Collection<T[K]> };

  // B2 — validate that every `t.ref("...")` target is a declared
  // collection in this same schema map. The TS `Tables<S>` constraint
  // catches the canonical case at compile-time; this runtime check is
  // the safety net for `t.ref("x" as any)` escapes and ensures we never
  // emit DDL that references a non-existent table (which would otherwise
  // fail later with a confusing Postgres error).
  validateRefTargets(schemas);

  // B2 — pass `skipRegister=true` so `model()` doesn't eagerly fire
  // `registerModel` itself. `createDb` runs the registrations in
  // topological order below, so parent tables (referenced via
  // `t.ref(...)`) get created before child tables. We can't strip
  // `registerModel` by wrapping `native` because env.db is a
  // `#[v8_class]` instance whose other methods (`.collection`,
  // `.beginTransaction`, …) need the original `this` binding —
  // `Object.create(env.db)` would lose internal-field access and
  // method calls would throw "Illegal invocation".
  for (const [name, rawSchema] of Object.entries(schemas)) {
    // Unwrap SchemaBuilder to extract per-collection options
    const isBuilder = rawSchema instanceof SchemaBuilder;
    // C2 — a top-level `t.union(...)` IS a valid schema input. `model()`
    // calls `normalizeSchema` which now recognises the TypeBuilder and
    // expands the union into flat columns. We pass it through unchanged.
    const isUnion = rawSchema instanceof TypeBuilder;
    const fields = isBuilder ? rawSchema.fields : rawSchema;
    const softDelete = isBuilder ? rawSchema.options.softDelete : false;
    const versioning = isBuilder ? rawSchema.options.versioning : false;
    (collections as Record<string, Collection<SchemaInput>>)[name] =
      model(
        name,
        // The TypeBuilder branch can't be cast to Record<string, unknown>
        // safely, but `model()` -> `normalizeSchema` accepts either form.
        (isUnion ? fields : fields) as Record<string, unknown>,
        native,
        namingStrategy,
        softDelete,
        versioning,
        /* skipRegister */ true,
      );
  }

  // Chain registerModel calls in topological order so parent tables
  // (referenced via `t.ref(...)`) are created before child tables. The
  // native orchestrator's advisory lock makes concurrent calls safe
  // but not deterministic: whichever orchestrator acquires the lock
  // first runs first, and a child running before its parent fails
  // inline-FK CREATE TABLE. Sequencing in JS removes the race
  // entirely.
  //
  // We also publish the chain on `globalThis.__zeroshipPlatformReady`
  // so the SSR dispatch shim can await it BEFORE opening the
  // request-scope auto-tx. pglite-socket's TCP proxy serializes
  // pglite queries per-connection-in-tx, so opening an auto-tx
  // connection while the orchestrator's registerModel connection
  // holds `pg_advisory_lock` deadlocks. Awaiting the chain pre-tx
  // keeps the two off the same socket window.
  const refOrder = topoSortByRefs(schemas as Record<string, unknown>);
  let chain: Promise<void> = Promise.resolve();
  for (const name of refOrder) {
    const col = (collections as Record<string, Collection<SchemaInput>>)[name];
    if (!col) continue;
    const rawSchema = schemas[name as keyof T];
    const fields =
      rawSchema instanceof SchemaBuilder ? rawSchema.fields : rawSchema;
    // Same normalisation `model()` does — TypeBuilder → plain field
    // defs (carries `type: "ref"` + `refTarget`), Mongoose-style →
    // field defs, union expansion. The Rust orchestrator expects this
    // shape.
    const normalized = normalizeSchema(fields as Parameters<typeof normalizeSchema>[0]);
    const dbSchema: ZeroshipDbSchema = {};
    for (const [key, def] of Object.entries(normalized)) {
      dbSchema[namingStrategy.toColumn(key)] = def as ZeroshipDbFieldDef;
    }
    chain = chain
      .catch(() => undefined)
      .then(() =>
        native.registerModel
          ? (native.registerModel(name, dbSchema) as Promise<void>)
          : Promise.resolve(),
      );
    (col as unknown as { _setReady(p: Promise<void> | null): void })._setReady(chain);
  }

  // Publish a "platform ready" promise the SSR dispatch shim awaits
  // before opening the auto-tx. Multiple createDb calls in the same
  // isolate (rare but possible if a user composes two app bundles)
  // are sequenced through one promise so the shim only needs to
  // `await` a single handle.
  const g = globalThis as { __zeroshipPlatformReady?: Promise<unknown> };
  const prev = g.__zeroshipPlatformReady ?? Promise.resolve();
  g.__zeroshipPlatformReady = prev.then(() => chain).catch(() => undefined);

  // Pre-cache TxCollection wrappers — stateless, reusable across transactions
  const txCollections = {} as { [K in keyof T]: TxCollection<T[K]> };
  for (const [name, col] of Object.entries(collections)) {
    (txCollections as Record<string, TxCollection<SchemaInput>>)[name] =
      createTxCollection(col as Collection<SchemaInput>);
  }

  const db = {
    ...collections,

    async transaction<R>(fn: (tx: { [K in keyof T]: TxCollection<T[K]> }) => Promise<R>, options?: TransactionOptions): Promise<Result<R>> {
      // BEGIN — the native runtime returns a Transaction wrapper
      // (v8_class) whose .commit() / .rollback() are explicit methods.
      // The wrapper's Drop auto-rollbacks if a thrown handler skips the
      // explicit teardown.
      const nativeAny = native as unknown as {
        beginTransaction?: (opts?: { isolationLevel?: string }) => Promise<{
          commit(): Promise<void>;
          rollback(): Promise<void>;
        }>;
      };
      if (typeof nativeAny.beginTransaction !== "function") {
        return err(new Error(
          "@zeroship/db: env.db.beginTransaction not available — " +
          "runtime is missing the Transaction v8_class surface.",
        ));
      }
      const tx = await nativeAny.beginTransaction(
        options?.isolationLevel ? { isolationLevel: options.isolationLevel } : undefined,
      );

      try {
        const result = await fn(txCollections);
        await tx.commit();
        return ok(result);
      } catch (e) {
        try {
          await tx.rollback();
        } catch {
          // Ignore rollback errors — wrapper's Drop also rolls back via
          // connection close as a safety net.
        }
        return err(e instanceof Error ? e : new Error(String(e)));
      }
    },
  };

  return db as Db<T>;
}
