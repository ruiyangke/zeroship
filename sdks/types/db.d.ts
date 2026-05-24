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
  [field: string]: ZeroshipDbFilterValue;
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
  | { $search?: string }
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
   * **P5.5 PR 7** — per-query unmask hint. Each column name in this
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
   * **P7 PR 5** — opt out of the default `AND deleted_at IS NULL`
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

  /** Find one document. Returns the row object or `null` when nothing matches. */
  findOne(filter: ZeroshipDbFilter, opts?: ZeroshipDbFindOpts): Promise<Record<string, unknown> | null>;

  /** Insert one document. Returns the inserted row. */
  insert(doc: Record<string, ZeroshipScalar | ZeroshipScalar[]>): Promise<Record<string, unknown>>;

  /** Insert multiple documents. Returns the inserted rows. */
  insertMany(docs: Record<string, ZeroshipScalar | ZeroshipScalar[]>[]): Promise<Record<string, unknown>[]>;

  /** Update one document. Returns the updated row or `null` when nothing matched. */
  updateOne(filter: ZeroshipDbFilter, update: ZeroshipDbUpdate): Promise<Record<string, unknown> | null>;

  /** Update multiple documents. Returns the count of affected rows. */
  updateMany(filter: ZeroshipDbFilter, update: ZeroshipDbUpdate): Promise<number>;

  /** Delete one document. Returns the deleted row or `null` when nothing matched.
   *
   *  **P7 PR 5** — on post-migration tables (those carrying the
   *  platform `deleted_at` column) this performs a SOFT delete:
   *  `UPDATE ... SET deleted_at = NOW()` and the returned row carries
   *  the populated `deleted_at`. On pre-migration tables it still
   *  performs a hard DELETE with an operator-side warning. Use
   *  `purge` for explicit hard-delete regardless of table state. */
  deleteOne(filter: ZeroshipDbFilter): Promise<Record<string, unknown> | null>;

  /** Delete multiple documents. Returns the count of affected rows.
   *
   *  **P7 PR 5** — same Path C semantics as `deleteOne`. */
  deleteMany(filter: ZeroshipDbFilter): Promise<number>;

  /** **P7 PR 5** — explicit hard-delete. Always emits `DELETE FROM ...`,
   *  regardless of the system-fields marker. */
  purge(filter: ZeroshipDbFilter): Promise<Record<string, unknown> | null>;

  /** **P7 PR 5** — bulk hard-delete. Returns the count of affected rows. */
  purgeMany(filter: ZeroshipDbFilter): Promise<number>;

  /** **P7 PR 5** — restore a soft-deleted row by clearing `deleted_at`.
   *  Refuses with `restore_unsupported_legacy_table` on pre-migration
   *  tables. */
  restore(filter: ZeroshipDbFilter): Promise<Record<string, unknown> | null>;

  /** **P7 PR 5** — bulk-restore. Returns the count of restored rows. */
  restoreMany(filter: ZeroshipDbFilter): Promise<number>;

  /** Upsert a document (insert or update on conflict). Returns the row.
   *  `opts.conflictFields` names the ON CONFLICT target columns — must
   *  be a non-empty array of column names; missing / empty rejects
   *  with `TypeError`. */
  upsert(
    doc: Record<string, ZeroshipScalar | ZeroshipScalar[]>,
    opts: { conflictFields: string[] },
  ): Promise<Record<string, unknown> | null>;

  /** Find a row matching `opts.conflictFields` or insert `doc`.
   *  Resolves with `{ row, created }` — `created` is `true` when the
   *  insert path fired, `false` when an existing row was returned.
   *  `opts.conflictFields` must be a non-empty array of column names. */
  findOrCreate(
    doc: Record<string, ZeroshipScalar | ZeroshipScalar[]>,
    opts: { conflictFields: string[] },
  ): Promise<{ row: Record<string, unknown>; created: boolean }>;

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

  /**
   * **P4 PR 2** — unified vector / FTS search entry. Discriminated by
   * `args.vector` (pgvector path) or `args.text` (FTS — PR 3). Each
   * returned row carries a synthetic `_distance` (vector) or `_rank`
   * (FTS) column.
   */
  search(args: {
    vector?: number[];
    text?: string;
    k?: number;
    metric?: "cosine" | "l2" | "innerProduct";
    column?: string;
    filter?: ZeroshipDbFilter;
  }): Promise<Record<string, unknown>[]>;

  /** Open a subscription bound to this collection. */
  openSubscription(): ZeroshipSubscription;
}

/**
 * A live transaction wrapper minted by `env.db.beginTransaction(opts?)`.
 * The wrapper's Drop auto-rollbacks if neither `.commit()` nor
 * `.rollback()` is called.
 *
 * Collections obtained via `tx.collection(name)` are tx-scoped (every
 * CRUD op routes through the transaction connection) and distinct
 * from `db.collection(name)`.
 */
interface ZeroshipTransaction {
  /** Mint a tx-scoped Collection wrapper. Identity is cached per
   *  name on this transaction. */
  collection(name: string): ZeroshipCollection;
  /** Commit the transaction. Idempotent — resolves void on a
   *  second call instead of rejecting. */
  commit(): Promise<void>;
  /** Rollback the transaction. Idempotent — resolves void on a
   *  second call instead of rejecting. */
  rollback(): Promise<void>;
}

/**
 * A live migration-run wrapper minted by `env.db.migrations.start(spec)`.
 * Holds the (app_id, name, collection) triple plus the session-scoped
 * Postgres advisory lock that fences concurrent runs. The wrapper's
 * Weak finalizer auto-cancels if the wrapper is GC'd without an
 * explicit terminal `commitBatch(isDone=true, ...)`.
 *
 * `status` / `cancel` / `reset` here operate on this exact run; the
 * `env.db.migrations.status({name, collection})` /
 * `.cancel({name, collection})` / `.reset({name, collection})` methods
 * operate by name+collection and don't hold the advisory lock.
 */
/**
 * Status snapshot returned by `ZeroshipMigration.status()` and
 * `ZeroshipMigrations.status(spec)`. Mirrors the audit-row shape; the
 * `@zeroship/migrations` SDK maps it to its public
 * `MigrationStatusSnapshot` type.
 */
interface ZeroshipMigrationStatus {
  exists: boolean;
  status: string | null;
  cursor: number;
  processed: number;
  deadLetterPks: number[];
  isDone: boolean;
  error: string | null;
}

/**
 * Lifecycle: `start(spec)` mints the wrapper in the `active` state.
 * `commitBatch(isDone=true, ...)` / `cancel()` / `reset()` all drive
 * it to the `settled` state — subsequent `commitBatch` / `cancel` /
 * `reset` calls reject because the wrapper no longer holds the
 * advisory lock; `status()` continues to work (the audit row is
 * still observable by `(name, collection)` coordinates).
 */
interface ZeroshipMigration {
  status(): Promise<ZeroshipMigrationStatus>;
  cancel(): Promise<void>;
  reset(): Promise<void>;

  /**
   * Fetch the next batch of rows after `cursor`. Resolves with the
   * row array; each row is a plain object keyed by column name.
   *
   * `cursor` / `batchSize` must be finite, integer-valued numbers in
   * the `i64` range; out-of-range values reject with a `RangeError`.
   */
  fetchBatch(cursor: number, batchSize: number): Promise<Record<string, unknown>[]>;

  /**
   * Commit one batch of per-row updates. The spec is walked from V8
   * directly — no `JSON.stringify` on the SDK side. If `isDone=true`,
   * drives the audit row to `terminalStatus` and releases the
   * advisory lock. Resolves void; rejects with the underlying
   * Postgres error on failure.
   */
  commitBatch(spec: {
    updates: { id: number; set: Record<string, unknown> }[];
    deadLetterPks: number[];
    nextCursor: number;
    processedTotal: number;
    isDone: boolean;
    terminalStatus?: string;
    errorMessage?: string;
  }): Promise<void>;
}

/** One event emitted by `ZeroshipSubscription.next()`. */
type ZeroshipSubscriptionEvent =
  | {
      kind: "change";
      op: "insert" | "update" | "delete";
      collection: string;
      pk: number | null;
      columns: string[];
    }
  | { kind: "resync" }
  | { kind: "closed" };

/**
 * A live subscription wrapper minted by `env.db.openSubscription(name)`.
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
  next(): Promise<ZeroshipSubscriptionEvent | null>;
  /** Idempotent synchronous teardown — see comment above. */
  close(): void;
}

/**
 * The `Migrations` namespace surfaced as `env.db.migrations`. Two
 * concerns:
 * - **Running** a migration: `start(spec)` mints a Migration wrapper.
 * - **Observing / controlling** an already-persisted migration row by
 *   coordinates: `status(spec)` / `cancel(spec)` / `reset(spec)`.
 */
interface ZeroshipMigrations {
  start(spec: {
    name: string;
    collection: string;
    dryRun?: boolean;
    reset?: boolean;
  }): Promise<ZeroshipMigration>;
  status(spec: { name: string; collection: string }): Promise<ZeroshipMigrationStatus>;
  cancel(spec: { name: string; collection: string }): Promise<void>;
  reset(spec: { name: string; collection: string }): Promise<void>;
}

// ---------------------------------------------------------------------------
// Db entry point — env.db
// ---------------------------------------------------------------------------

/**
 * Operator-facing replication namespace, surfaced as
 * `env.db.replication`. Apps don't call these — the deploy
 * orchestrator / control plane does.
 */
interface ZeroshipReplication {
  /**
   * Idempotently provision the per-app Postgres publication +
   * logical replication slot. Returns a JSON `SetupOutcome`
   * (`{publication, slot, created, confirmedFlushLsn}`). Requires
   * `wal_level=logical` on the server.
   */
  setup(opts?: { appId?: string }): Promise<string>;

  /**
   * Run the C1 watchdog query against `pg_replication_slots`.
   * Returns a JSON array of slot health records `[{slot, active,
   * restartLsn, confirmedFlushLsn, lagBytes, walStatus}]`.
   */
  watchdog(): Promise<string>;

  /**
   * Drop replication slots that have been inactive for at least
   * `opts.inactiveSeconds` (default 3600). Returns the names of
   * dropped slots as a JSON array. Apps whose slot was reaped see
   * a `resync` event on next subscriber attach.
   */
  dropAbandoned(opts?: { inactiveSeconds?: number }): Promise<string>;
}

/**
 * The `zeroship.db` namespace surfaced as `env.db` on every isolate.
 * Every operation lives on the Db v8_class instance — schema
 * registration, collection mint, transaction open, subscription open,
 * the migrations / replication sub-namespaces.
 */
interface ZeroshipDb {
  /**
   * Register a model — creates table and columns if not exist. The
   * optional third argument carries named multi-column indexes
   * declared via `schema(...).index(name, fields)`; each materialises
   * as a CONCURRENTLY-built Postgres index named
   * `"<collection>__<name>"`.
   */
  registerModel(
    collection: string,
    schema: ZeroshipDbSchema,
    indexes?: ZeroshipDbNamedIndex[],
  ): Promise<void>;

  /**
   * **P5.5 PR 4** — single-cell unmask round-trip. Reachable from
   * `MaskedValue.unmask()`; not part of the creator-facing SDK
   * surface.
   */
  unmaskField(args: {
    collection: string;
    row_pk: string;
    column: string;
    actor?: Record<string, unknown> | null;
    reason?: string;
  }): Promise<{ plaintext: string }>;

  /**
   * **P5.5 PR 7** — bulk unmask round-trip. Authorisation is atomic:
   * a single denied (rowPk, column) pair refuses the whole call with
   * `bulk_unmask_partial_unauthorized`. Reachable from
   * `Collection.bulkUnmask` and `MaskedValue.unmask(columns, opts)`.
   */
  bulkUnmaskFields(args: {
    collection: string;
    items: ReadonlyArray<{ rowPk: string; columns: readonly string[] }>;
    actor?: Record<string, unknown> | null;
    reason?: string;
  }): Promise<{ results: Record<string, Record<string, string>> }>;

  /**
   * **P5.5 PR 5** — install the per-app mask policy. Called once at
   * app boot from `defineMaskPolicy()`'s pending-slot drain.
   */
  setMaskPolicy(policy: Record<string, string[]>): Promise<Record<string, never>>;

  /**
   * Mint (or return the cached) Collection wrapper for `name`. Identity
   * is cached on the Db wrapper so repeated calls with the same name
   * return the same JS object — the SDK relies on this for per-name
   * lazy resolution.
   */
  collection(name: string): ZeroshipCollection;

  /**
   * Begin a transaction. `opts.isolationLevel` accepts
   * `"readCommitted"` / `"repeatableRead"` / `"serializable"` (and the
   * matching SQL strings). Returns a Transaction wrapper whose
   * `.commit()` / `.rollback()` are explicit methods. The wrapper's
   * Drop auto-rollbacks via connection close if neither is called.
   */
  beginTransaction(opts?: {
    isolationLevel?: "readCommitted" | "repeatableRead" | "serializable";
  }): Promise<ZeroshipTransaction>;

  /**
   * Mint (or return the cached) Migrations namespace wrapper. Identity
   * is cached on the Db wrapper so repeated reads of `env.db.migrations`
   * return the same JS object.
   *
   * - `.start(spec)` acquires a Postgres advisory lock and returns a
   *   live Migration wrapper that drives `fetchBatch` / `commitBatch`.
   * - `.status(spec)` / `.cancel(spec)` / `.reset(spec)` operate on a
   *   migration row by `{name, collection}` and don't acquire the
   *   advisory lock — safe to call while another worker has an active
   *   run.
   */
  migrations: ZeroshipMigrations;

  /**
   * Open a subscription on `collection`. Returns a Subscription wrapper.
   * Synchronous — calling it does not allocate any Postgres state; the
   * wrapper merely registers a slot in the per-isolate broker routing
   * table.
   *
   * Today the events come from local mutations in the same isolate
   * (coarse-grained, every change to `collection`). The proposal's
   * read-set narrowing + cross-worker WAL pickup are P8b / P8a.2.
   */
  openSubscription(collection: string): ZeroshipSubscription;

  /**
   * Auto-spawn the supervised WAL consumer for this app. Idempotent.
   * `opts.appId` overrides the current app context for the operator
   * path. Resolves once the publication + slot are durable.
   */
  startReplicationConsumer(opts?: string): Promise<string>;

  /**
   * Operator-facing replication ops. Identity-cached: repeated reads
   * of `env.db.replication` return the same JS object.
   */
  replication: ZeroshipReplication;
}
