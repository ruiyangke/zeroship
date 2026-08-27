/**
 * Database primitives (zeroship.db.*) — v2 surface.
 *
 * The runtime exposes `env.db` as a Db v8_class instance with a small set
 * of entry-point methods. Per-collection CRUD lives on the Collection
 * wrapper returned by `env.db.collection(name)`. Transactions, migration
 * runs, and reactive subscriptions are also separate wrappers. The
 * flat-method surface from v1 was removed in 2026-05.
 */

// ---------------------------------------------------------------------------
// Filter types
// ---------------------------------------------------------------------------

/** Filter object — MongoDB-style query operators, translated by SDK before native call. */
interface ZeroshipDbFilter {
  [field: string]: ZeroshipDbFilterValue | ZeroshipDbFilter[] | ZeroshipDbFilter | undefined;
  $and?: ZeroshipDbFilter[];
  $or?: ZeroshipDbFilter[];
  $not?: ZeroshipDbFilter;
}

/** A single filter field value — direct scalar, null, or operator object. */
type ZeroshipDbFilterValue =
  | ZeroshipScalar
  | { $eq?: ZeroshipScalar }
  | { $ne?: ZeroshipScalar }
  | { $gt?: string | number }
  | { $gte?: string | number }
  | { $lt?: string | number }
  | { $lte?: string | number }
  | { $in?: ZeroshipScalar[] }
  | { $nin?: ZeroshipScalar[] }
  | { $like?: string }
  | { $ilike?: string }
  | { $exists?: boolean };

// ---------------------------------------------------------------------------
// Update types
// ---------------------------------------------------------------------------

/** Update object — per-field operators (SDK translates top-level $set/$inc to this form). */
interface ZeroshipDbUpdate {
  [field: string]: ZeroshipDbUpdateValue;
}

/** A single update field value — direct scalar or operator object. */
type ZeroshipDbUpdateValue =
  | ZeroshipScalar
  | { $set?: ZeroshipScalar }
  | { $inc?: number }
  | { $dec?: number }
  | { $mul?: number }
  | { $push?: ZeroshipScalar }
  | { $pull?: ZeroshipScalar }
  | { $addToSet?: ZeroshipScalar };

// ---------------------------------------------------------------------------
// Query options
// ---------------------------------------------------------------------------

/** Find query options — key names must match what the Rust callback reads. */
interface ZeroshipDbFindOpts {
  limit?: number;
  offset?: number;
  orderBy?: Record<string, 1 | -1>;
  select?: string[];
  /**
   * per-query unmask hint. Each column name in this
   * array is promoted from `MaskedValue<T>` to bare plaintext on the
   * returned row(s). Authorisation is checked upfront against the
   * per-app mask policy; a single unauthorised column rejects the
   * whole find with `unmask_not_permitted`.
   *
   * Pass `actor` alongside `unmask` to identify the role the policy
   * lookup should consult. The optional `unmaskReason` flows into
   * the `__zeroship_audit_unmask.reason` column (prefixed with
   * `[query_hint]`) so audit-log readers can correlate hint
   * dispatches with their business context.
   */
  unmask?: string[];
  actor?: Record<string, unknown>;
  unmaskReason?: string;
  /**
   * opt out of the default `AND deleted_at IS NULL`
   * auto-filter. When `true`, soft-deleted rows participate in the
   * result set. Default omitted means "filter soft-deleted out" on
   * post-migration tables (no-op on pre-migration tables where the
   * column doesn't exist).
   */
  include_deleted?: boolean;
}

// ---------------------------------------------------------------------------
// Aggregate types
// ---------------------------------------------------------------------------

/** Aggregate pipeline stage. */
type ZeroshipDbAggregateStage =
  | { $match: ZeroshipDbFilter }
  | { $group: ZeroshipDbGroupStage }
  | { $having: ZeroshipDbFilter }
  | { $sort: Record<string, 1 | -1> }
  | { $limit: number };

/** Group stage — `by` is the group key, other fields are accumulators. */
interface ZeroshipDbGroupStage {
  by?: string | string[];
  [agg: string]: ZeroshipDbAccumulator | string | string[] | undefined;
}

/** Accumulator expression inside a $group stage. */
type ZeroshipDbAccumulator =
  | { $count: true }
  | { $sum: string }
  | { $avg: string }
  | { $min: string }
  | { $max: string }
  | { $first: string };

// ---------------------------------------------------------------------------
// Schema types (zeroship.db.registerModel)
// ---------------------------------------------------------------------------

/**
 * Supported field type names. Includes:
 * - core primitives + `"json"` + `"array"`
 * - `"ref"` (B2 typed FKs)
 * - `"object"` (D2 nested validators, stored as JSONB)
 * - `"calendarDate"` (D3, stored as Postgres DATE)
 * - `"literal"` and `"union"` (C2 discriminated unions; a top-level
 *   union is flattened by the SDK into discrete columns before it
 *   reaches the native driver, but the discriminator column still
 *   carries the `variants` metadata for DDL CHECK emission.)
 */
type ZeroshipDbTypeName =
  | "string"
  | "number"
  | "boolean"
  | "date"
  | "json"
  | "array"
  | "ref"
  | "object"
  | "calendarDate"
  | "literal"
  | "union";

/** Supported primitive item type names (for array fields). */
type ZeroshipDbPrimitiveTypeName = "string" | "number" | "boolean" | "date" | "json";

/** B2 — foreign-key action policy emitted into FK DDL. */
type ZeroshipDbFkAction = "restrict" | "cascade" | "set null" | "no action";

/** Normalized schema field definition passed to registerModel. */
interface ZeroshipDbFieldDef {
  type: ZeroshipDbTypeName;
  items?: ZeroshipDbPrimitiveTypeName;
  required?: boolean;
  unique?: boolean;
  index?: boolean;
  default?: ZeroshipScalar | Record<string, unknown>;
  min?: number;
  max?: number;
  enum?: (string | number)[];
  pattern?: RegExp;
  /** B2 — target collection name for `t.ref("...")`. Present iff `type === "ref"`. */
  refTarget?: string;
  /** B2 — ON DELETE policy. Default at DDL emit time: "restrict". */
  onDelete?: ZeroshipDbFkAction;
  /** B2 — ON UPDATE policy. Default at DDL emit time: "restrict". */
  onUpdate?: ZeroshipDbFkAction;
  /** B2 — whether FK is `DEFERRABLE INITIALLY DEFERRED`. Default: true. */
  deferrable?: boolean;
  /** D2 — nested-object shape (JSONB column, validated app-side). */
  shape?: Record<string, ZeroshipDbFieldDef>;
  /** C2 — literal value for `type === "literal"` (or per-variant disc field). */
  literalValue?: string | number | boolean;
  /** C2 — per-variant shape map for a flat-expanded union discriminator. */
  variants?: Record<string, ZeroshipDbFieldDef>[];
  /**
   * C2 — discriminator marker. On a `type === "union"` def this is the
   * field name; on a flat-expanded primitive column it's the literal
   * `"__discriminator__"` sentinel telling the DDL emitter to attach
   * per-variant CHECK constraints.
   */
  discriminator?: string;
}

/** Normalized schema — field name → definition. */
type ZeroshipDbSchema = Record<string, ZeroshipDbFieldDef>;

/**
 * Named multi-column index declaration carried alongside the schema in
 * the `registerModel` wire format. The orchestrator materialises each
 * entry as `CREATE INDEX CONCURRENTLY IF NOT EXISTS "<table>__<name>"`.
 * `fields` carries column names (already mapped through the naming
 * strategy by the SDK), in declared order.
 */
interface ZeroshipDbNamedIndex {
  name: string;
  fields: string[];
  unique?: boolean;
}

// ---------------------------------------------------------------------------
// Wrapper v8_classes — the v2 native surface.
// ---------------------------------------------------------------------------

/**
 * A typed Collection wrapper minted by `env.db.collection(name)`.
 * Identity is cached on the Db wrapper — calling `.collection(name)`
 * twice with the same name returns the same JS object.
 *
 * Every CRUD method resolves a real JS value — no JSON.stringify
 * boundary. `updateMany` / `deleteMany` resolve with the raw integer
 * count of affected rows.
 */
interface ZeroshipCollection {
  /** Find multiple documents. Returns the row array. */
  find(filter: ZeroshipDbFilter, opts?: ZeroshipDbFindOpts): Promise<Record<string, unknown>[]>;

  /** Insert one document. Returns the inserted row. */
  insert(doc: Record<string, ZeroshipScalar | ZeroshipScalar[]>): Promise<Record<string, unknown>>;

  /** Insert multiple documents. Returns the inserted rows. */
  insertMany(docs: Record<string, ZeroshipScalar | ZeroshipScalar[]>[]): Promise<Record<string, unknown>[]>;

  /** Update one document. Returns the updated row or `null` when nothing matched.
   *
   *  renamed from `updateOne` to `update` to align with
   *  the SDK and Prisma/Convex singular-default convention. */
  update(filter: ZeroshipDbFilter, update: ZeroshipDbUpdate): Promise<Record<string, unknown> | null>;

  /** Update multiple documents. Returns the count of affected rows. */
  updateMany(filter: ZeroshipDbFilter, update: ZeroshipDbUpdate): Promise<number>;

  /** Delete one document. Returns the deleted row or `null` when nothing matched.
   *
   *  renamed from `deleteOne` to `delete`.
   *
   *  on post-migration tables (those carrying the
   *  platform `deleted_at` column) this performs a SOFT delete:
   *  `UPDATE ... SET deleted_at = NOW()` and the returned row carries
   *  the populated `deleted_at`. On pre-migration tables it still
   *  performs a hard DELETE with an operator-side warning. Use
   *  `purge` for explicit hard-delete regardless of table state. */
  delete(filter: ZeroshipDbFilter): Promise<Record<string, unknown> | null>;

  /** Delete multiple documents. Returns the count of affected rows.
   *
   *  same Path C semantics as `delete`. */
  deleteMany(filter: ZeroshipDbFilter): Promise<number>;

  /** explicit hard-delete. Always emits `DELETE FROM ...`,
   *  regardless of the system-fields marker. */
  purge(filter: ZeroshipDbFilter): Promise<Record<string, unknown> | null>;

  /** bulk hard-delete. Returns the count of affected rows. */
  purgeMany(filter: ZeroshipDbFilter): Promise<number>;

  /** restore a soft-deleted row by clearing `deleted_at`.
   *  Refuses with `restore_unsupported_legacy_table` on pre-migration
   *  tables. */
  restore(filter: ZeroshipDbFilter): Promise<Record<string, unknown> | null>;

  /** bulk-restore. Returns the count of restored rows. */
  restoreMany(filter: ZeroshipDbFilter): Promise<number>;

  /** Upsert a document (insert or update on conflict). Returns the row.
   *  `opts.conflictFields` names the ON CONFLICT target columns — must
   *  be a non-empty array of column names; missing / empty rejects
   *  with `TypeError`.
   *
   *  `findOrCreate` was removed; callers use `upsert`
   *  directly. If the SDK consumer needs the legacy `{row, created}`
   *  envelope, do an explicit `find(filter).first()` first, branch on
   *  `null`, and decide. */
  upsert(
    doc: Record<string, ZeroshipScalar | ZeroshipScalar[]>,
    opts: { conflictFields: string[] },
  ): Promise<Record<string, unknown> | null>;

  /** Count documents matching `filter`. `opts.include_deleted: true`
   *  (P7 PR 5) opts out of the auto soft-delete filter. */
  count(
    filter: ZeroshipDbFilter,
    opts?: { include_deleted?: boolean },
  ): Promise<number>;

  /** Get distinct values for `opts.field` across rows matching
   *  `filter`. `opts.include_deleted: true` (P7 PR 5) opts out of the
   *  soft-delete auto-filter. */
  distinct(
    filter: ZeroshipDbFilter,
    opts: { field: string; include_deleted?: boolean },
  ): Promise<(string | number | boolean | null)[]>;

  /** Run an aggregation pipeline. `opts.include_deleted: true` (P7
   *  PR 5) opts out of the soft-delete auto-`$match`. */
  aggregate(
    pipeline: ZeroshipDbAggregateStage[],
    opts?: { include_deleted?: boolean },
  ): Promise<Record<string, unknown>[]>;

  /** Vector search. Returned rows carry a synthetic `_distance` column. */
  search(args: {
    vector: number[];
    k?: number;
    metric?: "cosine" | "l2" | "innerProduct";
    column?: string;
    filter?: ZeroshipDbFilter;
  }): Promise<Record<string, unknown>[]>;

  /** Open a subscription bound to this collection. */
  openSubscription(): ZeroshipSubscription;

  /**
   * single-cell unmask round-trip. The collection name
   * is inherited from this receiver (not passed in args). Reachable
   * from `Collection.unmaskField`; resolves with the bare plaintext
   * string. Granted AND denied dispatches both write an audit row.
   */
  unmaskField(
    rowPk: string,
    column: string,
    opts?: { actor?: Record<string, unknown> | null; reason?: string },
  ): Promise<string>;

  /**
   * bulk unmask round-trip (collection inherited from
   * this receiver). Authorisation is atomic: a single denied
   * (rowPk, column) pair refuses the whole call with
   * `bulk_unmask_partial_unauthorized`. Reachable from
   * `Collection.bulkUnmask`.
   */
  bulkUnmask(
    items: ReadonlyArray<{ rowPk: string; columns: readonly string[] }>,
    opts?: { actor?: Record<string, unknown> | null; reason?: string },
  ): Promise<{ results: Record<string, Record<string, string>> }>;
}

/**
 * The collections-only view handed to a `env.db.transaction(fn)`
 * callback (P9 PR 3).
 *
 * Each property is a tx-bound {@link ZeroshipCollection} — every CRUD op
 * routes through the open transaction connection automatically. There is
 * NO `commit` / `rollback` / `collection` method: the transaction
 * lifecycle is owned entirely by the native Rust orchestrator. Abort by
 * throwing inside the callback; commit by resolving. The native
 * `Transaction` v8_class (with explicit `.commit()` / `.rollback()` and a
 * GC-auto-rollback finalizer) was removed.
 */
interface ZeroshipTxView {
  [collection: string]: ZeroshipCollection;
}

/** One event emitted by a subscription's `next()`. */
type ZeroshipSubscriptionEvent =
  | {
      kind: "change";
      op: "insert" | "update" | "delete";
      collection: string;
      pk: string | null;
      columns: string[];
    }
  | { kind: "resync" }
  | { kind: "closed" };

/**
 * A live subscription wrapper minted by
 * `env.db.<collection>.openSubscription()` (P9 PR 1: the duplicate
 * `env.db.openSubscription(name)` entry point was removed).
 * Synchronous to mint — calling it does not allocate any Postgres state;
 * the wrapper merely registers a slot in the per-isolate broker routing
 * table. The wrapper's GC finalizer is the safety-net release.
 *
 * `next` (async) resolves with the next event, OR with the terminal
 * `{ kind: "closed" }` event exactly once, after which subsequent
 * polls resolve `null`. So a polling loop sees this sequence:
 *
 *   change* (any number) → resync? → closed → null → null → ...
 *
 * `null` means "subscription closed AND already drained" — the
 * `{ kind: "closed" }` event was returned on a previous poll. Callers
 * iterating with `for await` should break on `null` or on a
 * `kind === "closed"` payload; both are valid terminators.
 *
 * `close` is synchronous by design — a local state flip on the broker
 * entry with no I/O — even though `next` (which awaits the broker) is
 * async.
 */
interface ZeroshipSubscription {
  /** Resolves only after distributed change delivery is armed. */
  ready(): Promise<void>;
  next(): Promise<ZeroshipSubscriptionEvent | null>;
  /** Idempotent synchronous teardown — see comment above. */
  close(): void;
}

// `ZeroshipMigrations` and `ZeroshipReplication` moved to
// `@zeroship/bootstrap`'s framework-internal `internal.d.ts` (reached
// via `__platform.migrations` / `__platform.replication`, not
// `env.db.*`). Absent from this published surface.

// ---------------------------------------------------------------------------
// Db entry point — env.db
// ---------------------------------------------------------------------------

/**
 * The `zeroship.db` namespace surfaced as `env.db` on every isolate.
 * The creator-facing operations live on the Db v8_class instance —
 * collection mint and transaction open. The platform-internal entry
 * points (schema registration, mask policy, replication, migrations)
 * moved to the `__platform` capability handle in P9 PR 4 (§8) and are
 * not on this surface.
 */
interface ZeroshipDb {
  // the platform-internal entry points moved off `env.db`
  // to the `__platform` capability handle (reached only via a V8
  // private symbol; §8). Removed from this published surface:
  //   - `registerModel`         → `__platform.registerModel`
  //   - `setMaskPolicy`         → `__platform.setMaskPolicy`
  //   - `migrations` (getter)   → `__platform.migrations`
  //   - `replication` (getter)  → `__platform.replication`
  // Their type declarations live in `@zeroship/bootstrap`'s
  // framework-internal `internal.d.ts` (the `ZeroshipDbPlatform`
  // interface). Creator IDE hover on `env.db` no longer surfaces them.
  // `env.db.__platform` (string access) is actively refused at runtime
  // with `platform_internal_only`.
  //
  // `unmaskField` / `bulkUnmaskFields` moved off `Db` to
  // `Collection` (collection name inherited from the receiver). See
  // `ZeroshipCollection.unmaskField` / `.bulkUnmask`. `MaskedValue`
  // instances dispatch unmask natively from their own bound `_meta`.

  /**
   * Mint (or return the cached) Collection wrapper for `name`. Identity
   * is cached on the Db wrapper so repeated calls with the same name
   * return the same JS object — the SDK relies on this for per-name
   * lazy resolution.
   */
  collection(name: string): ZeroshipCollection;

  /**
   * Run `callback` inside a transaction (P9 PR 3 — native orchestrator).
   *
   * `callback` receives a collections-only {@link ZeroshipTxView}; the
   * returned promise resolves with the callback's result on **commit**
   * (callback resolved) and rejects with the callback's error on
   * **rollback** (callback threw / rejected). A `transaction(...)` call
   * made while a transaction is already active opens a `SAVEPOINT`
   * instead of a fresh `BEGIN` (nested-tx via implicit savepoint; depth
   * cap 8 → `savepoint_depth_exceeded`).
   *
   * `opts.isolationLevel` is a {@link ZeroshipIsolationLevel} (the SQL-spaced
   * lowercase form - `"read uncommitted"` | `"read committed"` |
   * `"repeatable read"` | `"serializable"`); honoured on the outermost
   * `BEGIN` only. This type used to declare its own three-value camelCase
   * literal (`"readCommitted" | "repeatableRead" | "serializable"`), which
   * disagreed with `ZeroshipIsolationLevel` on both spelling and count - it
   * dropped `readUncommitted` entirely even though the runtime accepts it
   * (measured 2026-08-10, task #245). The runtime's `normalize_isolation_level`
   * (`crates/plugin-db/src/v8_classes/db.rs`) also accepts the camelCase and
   * uppercase SQL forms, but `ZeroshipIsolationLevel` is the one published
   * type - see its doc comment in `shared.d.ts` - so this signature matches
   * that rather than inventing a second accepted-but-undocumented union.
   *
   * This is the low-level native primitive. The `@zeroship/db` SDK wraps
   * it as `env.db.transaction(fn): Promise<Result<R>>` (the
   * `Result`-returning creator API); the native promise rejects rather
   * than returning a `Result`.
   */
  transaction<R>(
    callback: (tx: ZeroshipTxView) => R | Promise<R>,
    opts?: {
      isolationLevel?: ZeroshipIsolationLevel;
    },
  ): Promise<R>;

  // `migrations` and
  // `replication` moved to the `__platform` capability handle (see the
  // note at the top of this interface and `ZeroshipDbPlatform` in
  // `@zeroship/bootstrap`'s `internal.d.ts`). They are no longer on the
  // published `env.db` surface.
}
