/**
 * Type-only public surface for the database SDK — the types user code uses
 * to annotate handlers (`env.db.users` is a `Collection<...>`, the
 * transaction callback receives a `TxCollection`, etc).
 *
 * Runtime installation belongs to `@zeroship/bootstrap`; this package
 * exposes the creator-facing database API and types.
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

import type { Collection } from "./collection";
import type { NativeCollection } from "./native";
import type { AliasedCollection, ReadFrom } from "./read";
import type { LiveOptions, LiveQuery } from "./live";
import type { PaginationResult } from "./query";
import type {
  PlainObject,
  Actor,
  DistinctField,
  ExactWithSpec,
  GeoField,
  Result,
  Row,
  RowId,
  RowInput,
  SelectableField,
  SelectSpec,
  SortSpec,
  SortInput,
  UpsertOptions,
  UpdateExpression,
  VectorField,
  Filter,
  IsolationLevel,
  WithSpec,
  WithRelations,
  SchemaBuilder,
  TypeBuilder,
} from "./types";

// ---------------------------------------------------------------------------
// Schema-shape input (used by bootstrap's installSchema, also exposed
// here so user code can reference it when typing the entry's default
// export)
// ---------------------------------------------------------------------------

/**
 * Schema definition — plain fields, schema() builder with options, or
 * a top-level `t.union(...)` whose row shape is a discriminated union
 * (proposal §C2). The TypeBuilder form is type-erased to
 * type-erased builder forms here so the conditional in `UnwrapSchema`
 * can distribute over the union.
 */
// eslint-disable-next-line @typescript-eslint/no-explicit-any
export type SchemaInput =
  | Record<string, unknown>
  | SchemaBuilder<any>
  | TypeBuilder<unknown, any, any, any, any>;

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
  as<const A extends string>(alias: A): AliasedCollection<Row<S>, A>;
  insert(row: RowInput<S>): Promise<Row<S>>;
  insertMany(rows: RowInput<S>[]): Promise<Row<S>[]>;
  get<K extends string & keyof Row<S>>(
    idOrFilter: RowId<S> | Filter<S>,
    opts: {
      select: K[];
      orderBy?: SortSpec<S>;
      actor?: Actor;
      unmask?: (string & keyof Row<S>)[];
      unmaskReason?: string;
    },
  ): Promise<Pick<Row<S>, K> | null>;
  get<const W extends WithSpec<S>>(
    idOrFilter: RowId<S> | Filter<S>,
    opts: {
      with: ExactWithSpec<S, W>;
      orderBy?: SortSpec<S>;
      actor?: Actor;
      unmask?: (string & keyof Row<S>)[];
      unmaskReason?: string;
    },
  ): Promise<(Omit<Row<S>, keyof W> & WithRelations<S, W, AllSchemas>) | null>;
  get(
    idOrFilter: RowId<S> | Filter<S>,
    opts?: {
      orderBy?: SortSpec<S>;
      actor?: Actor;
      unmask?: (string & keyof Row<S>)[];
      unmaskReason?: string;
    },
  ): Promise<Row<S> | null>;
  exists(filter: Filter<S>): Promise<boolean>;
  find<const W extends WithSpec<S>>(
    filter: Filter<S>,
    opts: {
      with: ExactWithSpec<S, W>;
      actor?: Actor;
      unmask?: (string & keyof Row<S>)[];
      unmaskReason?: string;
    },
  ): TxQuery<S, Omit<Row<S>, keyof W> & WithRelations<S, W, AllSchemas>, AllSchemas>;
  find(
    filter?: Filter<S>,
    opts?: {
      actor?: Actor;
      unmask?: (string & keyof Row<S>)[];
      unmaskReason?: string;
    },
  ): TxQuery<S, Row<S>, AllSchemas>;
  upsert(row: RowInput<S>, options: UpsertOptions<S>): Promise<Row<S>>;
  update(idOrFilter: RowId<S> | Filter<S>, patch: UpdateExpression<S>): Promise<Row<S> | null>;
  updateMany(filter: Filter<S>, patch: UpdateExpression<S>): Promise<{ count: number }>;
  delete(idOrFilter: RowId<S> | Filter<S>): Promise<Row<S> | null>;
  deleteMany(filter: Filter<S>): Promise<{ deletedCount: number }>;
  purge(idOrFilter: RowId<S> | Filter<S>): Promise<Row<S> | null>;
  purgeMany(filter?: Filter<S>): Promise<{ purgedCount: number }>;
  restore(idOrFilter: RowId<S> | Filter<S>): Promise<Row<S> | null>;
  restoreMany(filter?: Filter<S>): Promise<{ restoredCount: number }>;
  count(filter?: Filter<S>): Promise<number>;
  distinct<K extends DistinctField<S> & keyof Row<S>>(field: K, filter?: Filter<S>): Promise<Exclude<Row<S>[K], undefined>[]>;
  aggregate(pipeline: ZeroshipDbAggregateStage[]): Promise<PlainObject[]>;
  bulkUnmask(
    items: ReadonlyArray<{
      id: RowId<S>;
      columns: readonly (string & keyof Row<S>)[];
    }>,
    opts: { actor: import("./types").Actor; reason?: string },
  ): Promise<Map<RowId<S>, Record<string, unknown>>>;
  search(
    args: {
      vector: number[];
      k?: number;
      metric?: import("./types").VectorMetric;
      column?: VectorField<S>;
      filter?: Filter<S>;
    },
  ): Promise<(Row<S> & { _distance?: number })[]>;
  near(args: {
    field: GeoField<S>;
    point: { lat: number; lng: number };
    radius: number;
    filter?: Filter<S>;
    limit?: number;
  }): Promise<(Row<S> & { _distance_m: number })[]>;
};

/** Query inside a transaction — same chainable API but resolves to data directly */
export type TxQuery<
  S = PlainObject,
  P = Row<S>,
  AllSchemas extends Record<string, unknown> = Record<string, unknown>,
> = {
  sort(s: SortInput<S>): TxQuery<S, P, AllSchemas>;
  limit(n: number): TxQuery<S, P, AllSchemas>;
  skip(n: number): TxQuery<S, P, AllSchemas>;
  select<K extends SelectableField<S>>(field: K): TxQuery<S, Pick<Row<S>, K>, AllSchemas>;
  select<K extends SelectableField<S>>(fields: readonly K[]): TxQuery<S, Pick<Row<S>, K>, AllSchemas>;
  select<const Selection extends SelectSpec<S>>(
    fields: Selection,
  ): TxQuery<S, Pick<Row<S>, keyof Selection & keyof Row<S>>, AllSchemas>;
  after(id: RowId<S>): TxQuery<S, P, AllSchemas>;
  with<const W extends WithSpec<S>>(spec: ExactWithSpec<S, W>): TxQuery<S, Omit<P, keyof W> & WithRelations<S, W, AllSchemas>, AllSchemas>;
  paginate(opts: {
    cursor?: string | null;
    numItems: number;
  }): Promise<PaginationResult<P>>;
  /** First matching row or `null`. */
  first(): Promise<P | null>;
  /** Exactly one match; throws when none or multiple rows match. */
  unique(): Promise<P>;
  /** Last matching row in the current sort, or `null`. */
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
export type SchemaShape<T> =
  T extends SchemaBuilder<infer S> ? S :
  T extends TypeBuilder<infer U, any, any, any, any> ? U :
  T;

type UnwrapSchema<T> = SchemaShape<T>;

/** Read row shape for a plain schema, `schema({...})` builder, or top-level union builder. */
export type RowOf<T> = Row<SchemaShape<T>>;

/** Insert/upsert input shape for a plain schema, `schema({...})` builder, or top-level union builder. */
export type RowInputOf<T> = RowInput<SchemaShape<T>>;

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

type DbMethodName = keyof Object
  | "__platform"
  | "__proto__"
  | "collection"
  | "from"
  | "live"
  | "transaction";

type DirectCollections<T extends Record<string, SchemaInput>> = {
  [K in keyof T as K extends DbMethodName ? never : K]: Collection<UnwrapSchema<T[K]>, K & string, T>;
};

/** Collections available inside a transaction, including names that collide
 * with transaction-view methods. */
export type TransactionDb<T extends Record<string, SchemaInput>> = {
  [K in keyof T as K extends "collection" | "from" ? never : K]: TxCollection<UnwrapSchema<T[K]>, T>;
} & {
  collection<K extends string & keyof T>(name: K): TxCollection<UnwrapSchema<T[K]>, T>;
  from: ReadFrom<true>;
};

/**
 * The shape `installSchema` plants on `env.db` (the native handle) on
 * top of the per-collection wrappers. `transaction` is a thin
 * `Result`-wrapping shim over the native transaction orchestrator;
 * `live` wraps the subscription primitives.
 */
export type DbExtensions<T extends Record<string, SchemaInput>> = {
  collection<K extends string & keyof T>(name: K): NativeCollection;
  from: ReadFrom;
  transaction: <R>(fn: (tx: TransactionDb<T>) => Promise<R>, options?: TransactionOptions) => Promise<Result<R>>;
  /**
   * Reactive query layer. Runs `queryFn`, yields the initial result,
   * then re-runs and yields a fresh result on every change to any
   * table the `queryFn` reads.
   *
   * Every change to a watched table fires a rerun. The tables are auto-detected
   * by observing which `Collection.find/get/...` methods the
   * `queryFn` calls during its first execution. Pass `{ tables: [...] }`
   * to bypass auto-detection (e.g. when the queryFn doesn't go through
   * a Collection).
   *
   * Throws `code = "LIVE_IN_TRANSACTION"` if called inside
   * `db.transaction(tx => ...)`. Returns an AsyncIterableIterator with
   * an explicit `close()` method for teardown.
   */
  live: {
    <R>(queryFn: () => { then<TResult1 = Result<R[]>, TResult2 = never>(
      resolve?: ((value: Result<R[]>) => TResult1 | PromiseLike<TResult1>) | null,
      reject?: ((reason: unknown) => TResult2 | PromiseLike<TResult2>) | null
    ): Promise<TResult1 | TResult2> }, options?: LiveOptions): LiveQuery<R>;
    <R>(queryFn: () => Promise<R[]> | R[], options?: LiveOptions): LiveQuery<R>;
  };
};

/**
 * Combined surface — `Collections<T> & DbExtensions<T>`. The SDK no
 * longer materialises an object of this exact shape (everything is
 * planted on `env.db`); the type describes the union users observe
 * when they read off `env.db`.
 */
export type Db<T extends Record<string, SchemaInput>> = DirectCollections<T> & DbExtensions<T>;
