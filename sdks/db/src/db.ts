/**
 * Public type surface for the database SDK — the types user code uses
 * to annotate handlers (`env.db.users` is a `Collection<...>`, the
 * transaction callback receives a `TxCollection`, etc).
 *
 * Stage 7 of the refactor moved the runtime helpers (`installSchema`,
 * `model`, `normalizeSchema`, `validateRefTargets`, `topoSortByRefs`)
 * into `@zeroship/bootstrap`. Only the types stay here — `@zeroship/db`
 * is now purely user-facing. The bootstrap package consumes these
 * types via `@zeroship/db/internal` (one-way dependency: bootstrap →
 * db) so the API users see in autocomplete is decoupled from the
 * coordination internals.
 *
 * Usage in user code (typical):
 *   import { t, schema } from "@zeroship/db";
 *   import { env } from "zeroship";
 *
 *   export default {
 *     schema: {
 *       users: {
 *         email: t.string().required().unique(),
 *         name:  t.string().required().max(100),
 *       },
 *       todos: {
 *         userId: t.ref("users").required(),
 *         title:  t.string().required().min(1).max(200),
 *         done:   t.boolean().default(false),
 *       },
 *     },
 *   };
 *
 *   // Inside any procedure handler:
 *   const { data: user } = await env.db.users.insert({
 *     email: "alice@example.com",
 *     name:  "Alice",
 *   });
 *
 *   // Transactions — tx mirrors db, throws on error:
 *   const { data, error } = await env.db.transaction(async (tx) => {
 *     const u = await tx.users.insert({ email: "...", name: "..." });
 *     await tx.todos.insert({ userId: u.id, title: "buy milk" });
 *     return u;
 *   });
 *
 * Returns Result vs throws:
 * - `env.db.x.*` outside a transaction → `Promise<Result<T>>`.
 * - `tx.x.*` inside `env.db.transaction(tx => ...)` → `Promise<T>`, throws on error.
 * - `env.db.transaction(fn)` itself → `Promise<Result<R>>` — never throws.
 */

import type { Collection } from "./collection.js";
import type { LiveOptions, LiveQuery } from "./live.js";
import type {
  PlainObject,
  Result,
  Row,
  RowInput,
  UpdateExpression,
  Filter,
  IsolationLevel,
  WithSpec,
  WithRelations,
  SchemaBuilder,
  TypeBuilder,
} from "./types.js";

// ---------------------------------------------------------------------------
// Schema-shape input (used by bootstrap's installSchema, also exposed
// here so user code can reference it when typing the entry's default
// export)
// ---------------------------------------------------------------------------

/**
 * Schema definition — plain fields, schema() builder with options, or
 * a top-level `t.union(...)` whose row shape is a discriminated union
 * (proposal §C2). The TypeBuilder form is type-erased to
 * `TypeBuilder<unknown, any>` here so the conditional in `UnwrapSchema`
 * can distribute over the union.
 */
// eslint-disable-next-line @typescript-eslint/no-explicit-any
export type SchemaInput =
  | Record<string, unknown>
  | SchemaBuilder<Record<string, unknown>>
  | TypeBuilder<unknown, any>;

// ---------------------------------------------------------------------------
// Transaction surface — TxCollection / TxQuery / TransactionOptions
// ---------------------------------------------------------------------------

/**
 * A typed collection inside a transaction — same API as Collection but
 * throws on error instead of returning Result. Generic over schema
 * shape S.
 *
 * `AllSchemas` mirrors `Collection<S, N, AllSchemas>` — `installSchema`
 * passes the full schema map so `tx.x.find({...}, { with: { fk: true } })`
 * resolves the joined field to the target collection's `Row<...>` at the
 * type layer. Default `Record<string, unknown>` keeps direct `TxCollection`
 * consumers compiling (joined fields degrade to `PlainObject`).
 */
export type TxCollection<S = PlainObject, AllSchemas extends Record<string, unknown> = Record<string, unknown>> = {
  insert(row: RowInput<S>): Promise<Row<S>>;
  insertMany(rows: RowInput<S>[]): Promise<Row<S>[]>;
  get<K extends string & keyof Row<S>>(
    idOrFilter: string | Filter<S>,
    opts: { select: K[]; orderBy?: Record<string, 1 | -1> },
  ): Promise<Pick<Row<S>, K> | null>;
  get<W extends WithSpec>(
    idOrFilter: string | Filter<S>,
    opts: { with: W; orderBy?: Record<string, 1 | -1> },
  ): Promise<(Row<S> & WithRelations<S, W, AllSchemas>) | null>;
  get(
    idOrFilter: string | Filter<S>,
    opts?: { orderBy?: Record<string, 1 | -1> },
  ): Promise<Row<S> | null>;
  exists(filter: Filter<S>): Promise<boolean>;
  find<W extends WithSpec>(filter: Filter<S>, opts: { with: W }): TxQuery<S, Row<S> & WithRelations<S, W, AllSchemas>, AllSchemas>;
  find(filter?: Filter<S>): TxQuery<S, Row<S>, AllSchemas>;
  upsert(row: RowInput<S>, options: { conflictFields: (string & keyof Row<S>)[] }): Promise<Row<S>>;
  update(idOrFilter: string | Filter<S>, patch: UpdateExpression<S>): Promise<Row<S> | null>;
  updateMany(filter: Filter<S>, patch: UpdateExpression<S>): Promise<{ count: number }>;
  delete(idOrFilter: string | Filter<S>): Promise<Row<S> | null>;
  deleteMany(filter: Filter<S>): Promise<{ deletedCount: number }>;
  count(filter?: Filter<S>): Promise<number>;
  distinct(field: string & keyof Row<S>, filter?: Filter<S>): Promise<(string | number | boolean | null)[]>;
  aggregate(pipeline: ZeroshipDbAggregateStage[]): Promise<PlainObject[]>;
};

/** Query inside a transaction — same chainable API but resolves to data directly */
export type TxQuery<
  S = PlainObject,
  P = Row<S>,
  AllSchemas extends Record<string, unknown> = Record<string, unknown>,
> = {
  sort(s: Record<string, number> | string): TxQuery<S, P, AllSchemas>;
  limit(n: number): TxQuery<S, P, AllSchemas>;
  skip(n: number): TxQuery<S, P, AllSchemas>;
  select<K extends keyof Row<S> & string>(fields: K[]): TxQuery<S, Pick<Row<S>, K>, AllSchemas>;
  select(s: string | string[] | Record<string, number | boolean>): TxQuery<S, P, AllSchemas>;
  after(id: string): TxQuery<S, P, AllSchemas>;
  with<W extends WithSpec>(spec: W): TxQuery<S, P & WithRelations<S, W, AllSchemas>, AllSchemas>;
  /** **P9 PR 1** — terminal: first matching row or `null`. Throws inside
   *  the tx callback on a native error (tx unwraps Result). */
  first(): Promise<P | null>;
  /** **P9 PR 1** — strict terminal: exactly one match. Throws
   *  `NotFoundError` on 0 matches and `NotUniqueError` on >1. */
  unique(): Promise<P>;
  /** **P9 PR 1** — last matching row in the current sort, or `null`.
   *  Throws `InvalidOperationError` if no sort was set. */
  last(): Promise<P | null>;
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
// eslint-disable-next-line @typescript-eslint/no-explicit-any
type UnwrapSchema<T> =
  T extends SchemaBuilder<infer S> ? S :
  T extends TypeBuilder<infer U, any> ? U :
  T;

/**
 * The typed Collection map produced by `installSchema`. Indexed by
 * collection name; values are fully typed `Collection<...>` instances
 * carrying both the per-row shape (`UnwrapSchema<T[K]>`) and the full
 * schema map (`T`) so brand types and joined-relation types resolve
 * without extra casts at the call site.
 */
export type Collections<T extends Record<string, SchemaInput>> = {
  [K in keyof T]: Collection<UnwrapSchema<T[K]>, K & string, T>;
};

/**
 * The shape `installSchema` plants on `env.db` (the native handle) on
 * top of the per-collection wrappers. `transaction` is a thin
 * `Result`-wrapping shim over the native `env.db.transaction(fn)`
 * orchestrator (begin / commit / rollback / nested-savepoint all live in
 * Rust as of P9 PR 3); `live` wraps the subscription primitives.
 */
export type DbExtensions<T extends Record<string, SchemaInput>> = {
  transaction: <R>(fn: (tx: { [K in keyof T]: TxCollection<UnwrapSchema<T[K]>, T> }) => Promise<R>, options?: TransactionOptions) => Promise<Result<R>>;
  /**
   * Reactive query layer. Runs `queryFn`, yields the initial result,
   * then re-runs and yields a fresh result on every change to any
   * table the `queryFn` reads.
   *
   * v1 is coarse-grained: every change to a watched table fires a
   * rerun (no row-level filter narrowing). The tables are auto-detected
   * by observing which `Collection.find/get/...` methods the
   * `queryFn` calls during its first execution. Pass `{ tables: [...] }`
   * to bypass auto-detection (e.g. when the queryFn doesn't go through
   * a Collection).
   *
   * Throws `code = "live_in_transaction"` if called inside
   * `db.transaction(tx => ...)`. Returns an AsyncIterableIterator with
   * an explicit `close()` method for teardown.
   */
  live: <R>(queryFn: () => Promise<R[]> | { then(onFulfilled: (value: unknown) => unknown, onRejected?: (reason: unknown) => unknown): unknown }, options?: LiveOptions) => LiveQuery<R>;
};

/**
 * Combined surface — `Collections<T> & DbExtensions<T>`. The SDK no
 * longer materialises an object of this exact shape (everything is
 * planted on `env.db`); the type describes the union users observe
 * when they read off `env.db`.
 */
export type Db<T extends Record<string, SchemaInput>> = Collections<T> & DbExtensions<T>;
