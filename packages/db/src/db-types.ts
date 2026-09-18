/**
 * Type-only public surface for the database SDK — the types user code uses
 * to annotate handlers (`env.db.users` is a `Collection<...>`, the
 * transaction callback receives a `TxCollection`, etc).
 *
 * This package owns both its private runtime installer and the creator-facing
 * database API and types.
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

import type { NativeCollection, NativeDb } from "./native";
import type { NormalizedSchema } from "./schema";
import type {
  ReadColumn,
  ReadRow,
  ReadCondition,
  ReadOrder,
  Projection,
  Projected,
  Allowed,
} from "./read";
import type {
  PlainObject,
  Actor,
  DistinctField,
  ExactWithSpec,
  GeoField,
  Id,
  NamedIndexSpec,
  NamingStrategy,
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
  VectorMetric,
  Filter,
  IsolationLevel,
  WithSpec,
  WithRelations,
  SchemaBuilder,
  TypeBuilder,
} from "./types";

// ---------------------------------------------------------------------------
// Facade contract — the public shape of the runtime objects the host facade
// installs over native collections. The implementation classes live in the
// zeroship-data-v8 crate and `implement` these interfaces, so the public type
// contract and the runtime implementation are separate but checked against
// each other.
// ---------------------------------------------------------------------------

/** Read hints shared by the collection read methods. */
export type ReadHints<S> = {
  actor?: Actor;
  unmask?: (string & keyof Row<S>)[];
  unmaskReason?: string;
};

/** Constructor options accepted by the runtime `Collection`. */
export interface CollectionOptions {
  naming?: NamingStrategy;
  indexes?: readonly NamedIndexSpec[];
  schemas?: Readonly<Record<string, NormalizedSchema>>;
}

/** Page envelope returned by `Query.paginate()`. */
export type PaginationResult<R> = {
  page: R[];
  continueCursor: string;
  isDone: boolean;
};

/** Options for `db.live`. */
export interface LiveOptions {
  /**
   * Explicit list of tables to subscribe to. When provided, the
   * Collection-method auto-tracking is bypassed.
   */
  tables?: string[];
}

/** The handle returned by `db.live`. */
export interface LiveQuery<R> extends AsyncIterableIterator<R[]> {
  /** Idempotent. Cancels every underlying subscription. */
  close(): void;
}

/** A collection joined into a `db.from(...)` read. */
export interface AliasedCollection<T, A extends string = string> {
  readonly native: NativeDb;
  readonly collection: string;
  readonly alias: A;
  readonly columns: { [K in keyof T]-?: ReadColumn<T[K], A> };
  readonly toColumn: (field: string) => string;
  readonly toField: (field: string) => string;
  row(): ReadRow<T, A, false>;
  optionalRow(): ReadRow<T, A, true>;
}

/** Chainable read builder returned by a `db.from(...)` call. */
export interface ReadBuilder<P = never, Nullable extends string = never, Throws extends boolean = false> {
  innerJoin<T, A extends string>(source: AliasedCollection<T, A>, on: ReadCondition): ReadBuilder<P, Nullable, Throws>;
  leftJoin<T, A extends string>(source: AliasedCollection<T, A>, on: ReadCondition): ReadBuilder<P, Nullable | A, Throws>;
  where(where: ReadCondition): ReadBuilder<P, Nullable, Throws>;
  having(having: ReadCondition): ReadBuilder<P, Nullable, Throws>;
  groupBy(...columns: ReadColumn<unknown>[]): ReadBuilder<P, Nullable, Throws>;
  orderBy(...keys: ReadOrder[]): ReadBuilder<P, Nullable, Throws>;
  limit(limit: number): ReadBuilder<P, Nullable, Throws>;
  offset(offset: number): ReadBuilder<P, Nullable, Throws>;
  select<const Q extends Record<string, Projection>>(projection: Q & Allowed<Q, Nullable>): ReadBuilder<Projected<Q, Nullable>, Nullable, Throws>;
  all(): Promise<Throws extends true ? P[] : Result<P[]>>;
}

/** Entry point for joining collections in a read. */
export type ReadFrom<Throws extends boolean = false> = <T, A extends string>(source: AliasedCollection<T, A>) => ReadBuilder<never, never, Throws>;

/**
 * A typed collection — the full CRUD + aggregate surface exposed at
 * `env.db.<name>`. Generic over the raw schema shape `S`, the collection name
 * `N`, and the parent database's schema map `AllSchemas` (which resolves named
 * relation edges to their target row types).
 */
export interface Collection<
  S = PlainObject,
  N extends string = string,
  AllSchemas extends Record<string, unknown> = Record<string, unknown>,
> {
  readonly Id: Id<N, RowId<S>>;
  readonly RowInput: RowInput<S>;
  as<const A extends string>(alias: A): AliasedCollection<Row<S>, A>;
  insert(row: RowInput<S>): Promise<Result<Row<S>>>;
  insertMany(rows: RowInput<S>[]): Promise<Result<Row<S>[]>>;
  get<K extends string & keyof Row<S>>(
    idOrFilter: RowId<S> | Filter<S>,
    opts: { select: K[]; orderBy?: SortSpec<S> } & ReadHints<S>,
  ): Promise<Result<Pick<Row<S>, K> | null>>;
  get<const W extends WithSpec<S>, K extends string & keyof Row<S> = string & keyof Row<S>>(
    idOrFilter: RowId<S> | Filter<S>,
    opts: { with: ExactWithSpec<S, W>; select?: K[]; orderBy?: SortSpec<S> } & ReadHints<S>,
  ): Promise<Result<(Pick<Row<S>, K> & WithRelations<S, W, AllSchemas>) | null>>;
  get(
    idOrFilter: RowId<S> | Filter<S>,
    opts?: { orderBy?: SortSpec<S> } & ReadHints<S>,
  ): Promise<Result<Row<S> | null>>;
  exists(filter?: Filter<S>): Promise<Result<boolean>>;
  find<const W extends WithSpec<S>>(
    filter: Filter<S>,
    opts: { with: ExactWithSpec<S, W> } & ReadHints<S>,
  ): Query<S, Row<S> & WithRelations<S, W, AllSchemas>, AllSchemas>;
  find(filter?: Filter<S>): Query<S, Row<S>, AllSchemas>;
  upsert(row: RowInput<S>, options: UpsertOptions<S>): Promise<Result<Row<S>>>;
  update(idOrFilter: RowId<S> | Filter<S>, patch: UpdateExpression<S>): Promise<Result<Row<S> | null>>;
  updateMany(filter: Filter<S>, update: UpdateExpression<S>): Promise<Result<{ count: number }>>;
  delete(idOrFilter: RowId<S> | Filter<S>): Promise<Result<Row<S> | null>>;
  deleteMany(filter?: Filter<S>): Promise<Result<{ deletedCount: number }>>;
  purge(idOrFilter: RowId<S> | Filter<S>): Promise<Result<Row<S> | null>>;
  purgeMany(filter?: Filter<S>): Promise<Result<{ purgedCount: number }>>;
  restore(idOrFilter: RowId<S> | Filter<S>): Promise<Result<Row<S> | null>>;
  restoreMany(filter?: Filter<S>): Promise<Result<{ restoredCount: number }>>;
  count(filter?: Filter<S>): Promise<Result<number>>;
  distinct<K extends DistinctField<S> & keyof Row<S>>(field: K, filter?: Filter<S>): Promise<Result<Exclude<Row<S>[K], undefined>[]>>;
  aggregate(pipeline: ZeroshipDbAggregateStage[]): Promise<Result<PlainObject[]>>;
  bulkUnmask(
    items: ReadonlyArray<{
      id: RowId<S>;
      columns: readonly (string & keyof Row<S>)[];
    }>,
    opts: { actor: Actor; reason?: string },
  ): Promise<Result<Map<RowId<S>, Record<string, unknown>>>>;
  search(args: {
    vector: number[];
    k?: number;
    metric?: VectorMetric;
    column?: VectorField<S>;
    filter?: Filter<S>;
  }): Promise<Result<(Row<S> & { _distance?: number })[]>>;
  near(args: {
    field: GeoField<S>;
    point: { lat: number; lng: number };
    radius: number;
    filter?: Filter<S>;
    limit?: number;
  }): Promise<Result<(Row<S> & { _distance_m: number })[]>>;
}

/**
 * Chainable query object returned by `Collection.find()`. Collects
 * sort/limit/skip/select options lazily and executes via the native layer when
 * awaited. Generic over the raw schema shape `S`, the projected document shape
 * `P`, and the parent database's schema map `AllSchemas`.
 */
export interface Query<
  S = PlainObject,
  P = Row<S>,
  AllSchemas extends Record<string, unknown> = Record<string, unknown>,
> {
  sort(s: SortInput<S>): this;
  limit(n: number): this;
  skip(n: number): this;
  after(id: RowId<S>): this;
  with<const W extends WithSpec<S>>(
    spec: ExactWithSpec<S, W>,
  ): Query<S, Omit<P, keyof W> & WithRelations<S, W, AllSchemas>, AllSchemas>;
  select<K extends SelectableField<S>>(field: K): Query<S, Pick<Row<S>, K> & Omit<P, keyof Row<S>>, AllSchemas>;
  select<K extends SelectableField<S>>(fields: readonly K[]): Query<S, Pick<Row<S>, K> & Omit<P, keyof Row<S>>, AllSchemas>;
  select<const Selection extends SelectSpec<S>>(
    fields: Selection,
  ): Query<S, Pick<Row<S>, keyof Selection & keyof Row<S>> & Omit<P, keyof Row<S>>, AllSchemas>;
  paginate(opts: {
    cursor?: string | null;
    numItems: number;
  }): Promise<Result<PaginationResult<P>>>;
  first(): Promise<Result<P | null>>;
  unique(): Promise<Result<P>>;
  last(): Promise<Result<P | null>>;
  then<TResult1 = Result<P[]>, TResult2 = never>(
    resolve?: ((value: Result<P[]>) => TResult1 | PromiseLike<TResult1>) | null,
    reject?: ((reason: unknown) => TResult2 | PromiseLike<TResult2>) | null
  ): Promise<TResult1 | TResult2>;
}

// ---------------------------------------------------------------------------
// Schema-shape input used by the public collection and generated env types.
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
 * `AllSchemas` mirrors `Collection<S, N, AllSchemas>` — the host facade passes
 * the full schema map so `tx.x.find({...}, { with: { author: true } })`
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
  get<const W extends WithSpec<S>, K extends string & keyof Row<S> = string & keyof Row<S>>(
    idOrFilter: RowId<S> | Filter<S>,
    opts: {
      with: ExactWithSpec<S, W>;
      select?: K[];
      orderBy?: SortSpec<S>;
      actor?: Actor;
      unmask?: (string & keyof Row<S>)[];
      unmaskReason?: string;
    },
  ): Promise<(Pick<Row<S>, K> & WithRelations<S, W, AllSchemas>) | null>;
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
  ): TxQuery<S, Row<S> & WithRelations<S, W, AllSchemas>, AllSchemas>;
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
  select<K extends SelectableField<S>>(field: K): TxQuery<S, Pick<Row<S>, K> & Omit<P, keyof Row<S>>, AllSchemas>;
  select<K extends SelectableField<S>>(fields: readonly K[]): TxQuery<S, Pick<Row<S>, K> & Omit<P, keyof Row<S>>, AllSchemas>;
  select<const Selection extends SelectSpec<S>>(
    fields: Selection,
  ): TxQuery<S, Pick<Row<S>, keyof Selection & keyof Row<S>> & Omit<P, keyof Row<S>>, AllSchemas>;
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
  /** Outermost transaction isolation. SQLite accepts only serializable; omission uses the backend default. */
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
 * The typed Collection map produced by the host facade. Indexed by
 * collection name; values are fully typed `Collection<...>` instances
 * carrying both the per-row shape (`UnwrapSchema<T[K]>`) and the full
 * schema map (`T`) so brand types and joined-relation types resolve
 * without extra casts at the call site.
 */
export type Collections<T extends Record<string, SchemaInput>> = {
  [K in keyof T]: Collection<UnwrapSchema<T[K]>, K & string, T>;
};

type DbMethodName = keyof Object
  | "declareMaskPolicy"
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
 * The shape the host facade plants on `env.db` (the native handle) on
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
