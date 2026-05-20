/**
 * createDb — the primary entry point for @zeroship/db.
 *
 * Declares all models upfront, returns a fully-typed db object with
 * collections and transaction support. Every Collection method returns
 * `Result<T> = { data, error }` outside a transaction; inside
 * `db.transaction(tx => ...)` the `tx.<table>` adapter returns the
 * bare value and throws on error.
 *
 * Usage:
 *   import { createDb, t } from "@zeroship/db";
 *
 *   const db = createDb({
 *     users: {
 *       email: t.string().required().unique(),
 *       name:  t.string().required().max(100),
 *     },
 *     todos: {
 *       userId: t.ref("users").required(),
 *       title:  t.string().required().min(1).max(200),
 *       done:   t.boolean().default(false),
 *     },
 *   });
 *
 *   // CRUD — { data, error }
 *   const { data: user } = await db.users.insert({
 *     email: "alice@example.com",
 *     name:  "Alice",
 *   });
 *
 *   // Transaction — tx mirrors db, throws on error
 *   const { data, error } = await db.transaction(async (tx) => {
 *     const u = await tx.users.insert({ email: "...", name: "..." });
 *     await tx.todos.insert({ userId: u.id, title: "buy milk" });
 *     return u;
 *   });
 *
 *   // Each collection exposes typed `Id` and `RowInput` accessors:
 *   //   typeof db.users.Id        // Id<"users">
 *   //   typeof db.users.RowInput  // RowInput<usersSchema>
 *
 * Returns Result vs throws (worth knowing):
 * - `db.x.*` outside a transaction → `Promise<Result<T>>`.
 * - `tx.x.*` inside `db.transaction(tx => ...)` → `Promise<T>`, throws on error.
 * - `db.transaction(fn)` itself → `Promise<Result<R>>` — never throws.
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
  get<K extends string & keyof Row<S>>(
    idOrFilter: number | Filter<S>,
    opts: { select: K[]; orderBy?: Record<string, 1 | -1> },
  ): Promise<Pick<Row<S>, K> | null>;
  get(
    idOrFilter: number | Filter<S>,
    opts?: { orderBy?: Record<string, 1 | -1> },
  ): Promise<Row<S> | null>;
  exists(filter: Filter<S>): Promise<boolean>;
  find(filter?: Filter<S>): TxQuery<S, Row<S>>;
  upsert(row: RowInput<S>, options: { conflictFields: (string & keyof Row<S>)[] }): Promise<Row<S>>;
  update(idOrFilter: number | Filter<S>, patch: UpdateExpression<S>): Promise<Row<S> | null>;
  updateMany(filter: Filter<S>, patch: UpdateExpression<S>): Promise<{ count: number }>;
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
  [K in keyof T]: Collection<UnwrapSchema<T[K]>, K & string>
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

/**
 * Wraps the outer Collection so its CRUD methods return the bare value
 * and throw on error (vs Result<T>). The tx adapter does NOT re-route
 * the call to a different native handle — every call goes through the
 * same `Collection.{insert,update,...}` you'd use outside the tx, and
 * routing onto the transaction's Postgres connection happens in Rust
 * via the `TX_CONN` thread-local (`crates/plugin-db/src/callbacks.rs`
 * around the dispatch site). Two consequences worth knowing:
 *
 * - `db.users` and `tx.users` are the SAME Collection instance; the
 *   only difference is the surface adapter that strips Result.
 * - There is no `Collection<tx>` vs `Collection<db>` split in JS.
 *   Schema validation, naming, and the soft-delete / versioning
 *   plumbing all live on one object.
 */
function createTxCollection<S>(collection: Collection<S>): TxCollection<S> {

  // The TxCollection.get signature is two overloads (with/without
  // select); the runtime impl is a single function that delegates to
  // the underlying Collection — the overload-aware return type comes
  // from the TxCollection type, not the impl signature.
  async function getImpl(
    idOrFilter: number | Filter<S>,
    opts?: { select?: (string & keyof Row<S>)[]; orderBy?: Record<string, 1 | -1> },
  ): Promise<unknown> {
    const colAny = collection as unknown as {
      get(
        idOrFilter: number | Filter<S>,
        opts?: { select?: (string & keyof Row<S>)[]; orderBy?: Record<string, 1 | -1> },
      ): Promise<Result<Row<S> | null>>;
    };
    return unwrap(await colAny.get(idOrFilter, opts));
  }

  const tx: TxCollection<S> = {
    async insert(row: RowInput<S>) {
      return unwrap(await collection.insert(row));
    },
    async insertMany(rows: RowInput<S>[]) {
      return unwrap(await collection.insertMany(rows));
    },
    get: getImpl as TxCollection<S>["get"],
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

/** Wrap a Query to throw on error. The `select` overloads mirror the
 *  underlying `Query.select` so a typed-fields call narrows the result
 *  to `Pick<Row<S>, K>` — the runtime path stays untyped, but the
 *  generic signatures preserve the narrowing across the wrapper. */
function createTxQuery<S>(query: Query<S, Row<S>>): TxQuery<S, Row<S>> {
  function selectImpl(
    s: string | string[] | Record<string, number | boolean>,
  ): unknown {
    (query.select as (arg: unknown) => unknown)(s);
    // Both overloads return the same wrapper instance; only the type
    // narrows at the call boundary via the TxQuery type's signatures.
    return wrapped;
  }
  const wrapped: TxQuery<S, Row<S>> = {
    sort(s: Record<string, number> | string) { query.sort(s); return wrapped; },
    limit(n: number) { query.limit(n); return wrapped; },
    skip(n: number) { query.skip(n); return wrapped; },
    // The overloads on TxQuery.select carry the K[] narrowing; the
    // runtime implementation is a single function that delegates to
    // the underlying Query — the cast is local to the call site.
    select: selectImpl as TxQuery<S, Row<S>>["select"],
    after(id: number) { query.after(id); return wrapped; },
    then<TResult1 = Row<S>[], TResult2 = never>(
      resolve?: ((value: Row<S>[]) => TResult1 | PromiseLike<TResult1>) | null,
      reject?: ((reason: unknown) => TResult2 | PromiseLike<TResult2>) | null,
    ): Promise<TResult1 | TResult2> {
      return query.then(
        (result: Result<Row<S>[]>) => {
          if (result.error) throw result.error;
          return resolve ? resolve(result.data as Row<S>[]) : (result.data as unknown as TResult1);
        },
        reject,
      ) as Promise<TResult1 | TResult2>;
    },
  };
  return wrapped;
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
    const declaredIndexes = isBuilder ? rawSchema.indexes : [];
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
        declaredIndexes,
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
    const declaredIndexes =
      rawSchema instanceof SchemaBuilder ? rawSchema.indexes : [];
    const wireIndexes: ZeroshipDbNamedIndex[] = declaredIndexes.map((idx) => ({
      name: idx.name,
      fields: idx.fields.map((f) => namingStrategy.toColumn(f)),
      ...(idx.unique ? { unique: true } : {}),
    }));
    // Sequencing in JS removes the parent-before-child race, but the
    // chain MUST surface registerModel failures: a broken DDL for table
    // X means every CRUD call against X is doomed, and silently
    // skipping past the rejection (`.catch(() => undefined)`) just
    // pushes the failure into a confusing later-stage Postgres error.
    // Each iteration's `then(...)` runs only on fulfilment, so a
    // rejected chain short-circuits the remaining registrations; the
    // promise the Collection stores via `_setReady` then rejects on
    // first CRUD and `_run` converts that to `result.error`.
    chain = chain.then(() => {
      if (!native.registerModel) return Promise.resolve();
      const registerAny = native.registerModel as unknown as (
        collection: string,
        schema: ZeroshipDbSchema,
        indexes?: ZeroshipDbNamedIndex[],
      ) => Promise<void>;
      return registerAny(name, dbSchema, wireIndexes);
    });
    (col as unknown as { _setReady(p: Promise<void> | null): void })._setReady(chain);
  }

  // Publish a "platform ready" promise the SSR dispatch shim awaits
  // before opening the auto-tx. Multiple createDb calls in the same
  // isolate (rare but possible if a user composes two app bundles)
  // are sequenced through one promise so the shim only needs to
  // `await` a single handle.
  //
  // Consumers: `sdks/vite-plugin/src/rpc-registry.ts` (production
  // dispatch shim) and `sdks/vite-plugin/src/dev-bootstrap/index.ts`
  // (dev bootstrap). Both read this exact name; renaming it would
  // also need a coordinated rename there. The sigil is intentionally
  // namespaced (`__zeroship*`) to avoid colliding with user globals.
  //
  // The chain is published as-is — if registerModel rejects, the
  // dispatch shim's `await` rejects too, which is the correct signal
  // (the platform isn't actually ready and serving a request against
  // a broken DDL is worse than failing fast).
  const g = globalThis as { __zeroshipPlatformReady?: Promise<unknown> };
  const prev = g.__zeroshipPlatformReady ?? Promise.resolve();
  g.__zeroshipPlatformReady = prev.then(() => chain);

  // Pre-cache TxCollection wrappers — stateless, reusable across transactions.
  // Mirror the outer `Db<T>`'s `UnwrapSchema<T[K]>` normalisation so
  // `tx.x.insert(...)` infers identically to `db.x.insert(...)`.
  const txCollections = {} as { [K in keyof T]: TxCollection<UnwrapSchema<T[K]>> };
  for (const [name, col] of Object.entries(collections)) {
    (txCollections as Record<string, TxCollection<unknown>>)[name] =
      createTxCollection(col as Collection<unknown>);
  }

  const db = {
    ...collections,

    async transaction<R>(fn: (tx: { [K in keyof T]: TxCollection<UnwrapSchema<T[K]>> }) => Promise<R>, options?: TransactionOptions): Promise<Result<R>> {
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

      // Bypass the per-collection IdLoader for the duration of the
      // tx body — batched reads route through `TX_CONN` correctly,
      // but mixing a tx-active load with a non-tx load already
      // queued in the same microtask would blur the connection
      // boundary. Each Collection's `_txDepth` counter is bumped
      // here and decremented in the `finally` below.
      const collectionList = Object.values(collections).map(
        (c) => c as unknown as {
          _txDepth: number;
          _idLoader: { _drain(): Promise<void> } | null;
        },
      );
      for (const c of collectionList) {
        if (c._idLoader !== null) void c._idLoader._drain();
        c._txDepth += 1;
      }

      // Two failure modes carry different post-conditions:
      //
      // 1. The transaction body threw — nothing was committed; we
      //    rollback and surface the body's error. Side-effects already
      //    flushed inside the tx (writes against TX_CONN) are reverted
      //    on rollback.
      //
      // 2. The body completed but `tx.commit()` itself threw — the
      //    SQL `COMMIT` was attempted, and depending on where it
      //    failed (network drop after server-side commit, deadlock at
      //    commit, prepared-tx ambiguity, ...) the database state is
      //    indeterminate. Callers cannot safely treat this as "rolled
      //    back". Tag the error with `.code = "commit_failed_indeterminate"`
      //    so a higher layer can decide whether to retry, prompt the
      //    user, or fail the request.
      let bodyResult: R;
      try {
        bodyResult = await fn(txCollections);
      } catch (bodyErr) {
        try { await tx.rollback(); } catch { /* Drop covers it */ }
        for (const c of collectionList) c._txDepth -= 1;
        return err(bodyErr instanceof Error ? bodyErr : new Error(String(bodyErr)));
      }
      try {
        await tx.commit();
      } catch (commitErr) {
        try { await tx.rollback(); } catch { /* commit-half may make rollback a no-op */ }
        for (const c of collectionList) c._txDepth -= 1;
        const msg = commitErr instanceof Error ? commitErr.message : String(commitErr);
        const wrapped = Object.assign(
          new Error(`commit failed — transaction state indeterminate: ${msg}`, {
            cause: commitErr instanceof Error ? commitErr : undefined,
          }),
          { code: "commit_failed_indeterminate" as const },
        );
        return err(wrapped);
      }
      for (const c of collectionList) c._txDepth -= 1;
      return ok(bodyResult);
    },
  };

  return db as Db<T>;
}
