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

// ---------------------------------------------------------------------------
// Wrapper v8_classes — the v2 native surface.
// ---------------------------------------------------------------------------

/**
 * A typed Collection wrapper minted by `env.db.collection(name)`. All
 * CRUD methods return JSON strings (or null) from the native layer.
 * Identity is cached on the Db wrapper — calling `.collection(name)`
 * twice with the same name returns the same JS object.
 */
interface ZeroshipCollection {
  /** Find multiple documents. Returns JSON array string. */
  find(filter: ZeroshipDbFilter, opts: ZeroshipDbFindOpts): Promise<string>;

  /** Find one document. Returns JSON string or null. */
  findOne(filter: ZeroshipDbFilter, opts: ZeroshipDbFindOpts): Promise<string | null>;

  /** Insert one document. Returns JSON string of the inserted row. */
  insert(doc: Record<string, ZeroshipScalar | ZeroshipScalar[]>): Promise<string>;

  /** Insert multiple documents. Returns JSON array string. */
  insertMany(docs: Record<string, ZeroshipScalar | ZeroshipScalar[]>[]): Promise<string>;

  /** Update one document. Returns JSON string of the updated row or null. */
  updateOne(filter: ZeroshipDbFilter, update: ZeroshipDbUpdate): Promise<string>;

  /** Update multiple documents. Returns JSON string with { updated: N }. */
  updateMany(filter: ZeroshipDbFilter, update: ZeroshipDbUpdate): Promise<string>;

  /** Delete one document. Returns JSON string of the deleted row or null. */
  deleteOne(filter: ZeroshipDbFilter): Promise<string>;

  /** Delete multiple documents. Returns JSON string with { deleted: N }. */
  deleteMany(filter: ZeroshipDbFilter): Promise<string>;

  /** Upsert a document (insert or update on conflict). Returns JSON string of the row.
   *  `opts.conflictFields` names the ON CONFLICT target columns. */
  upsert(
    doc: Record<string, ZeroshipScalar | ZeroshipScalar[]>,
    opts: { conflictFields: string[] },
  ): Promise<string>;

  /** Count documents matching filter. Returns the integer as a JSON
   *  number string (`"42"`); the SDK parses it to a number directly. */
  count(filter: ZeroshipDbFilter): Promise<string>;

  /** Get distinct values for a field. Returns JSON array string. */
  distinct(field: string, filter: ZeroshipDbFilter): Promise<string>;

  /** Run an aggregation pipeline. Returns JSON array string. */
  aggregate(pipeline: ZeroshipDbAggregateStage[]): Promise<string>;
}

/**
 * A live transaction wrapper minted by `env.db.beginTransaction(level?)`.
 * Subsequent CRUD ops issued on Collection wrappers in the same isolate
 * tick run on the transaction's connection. The wrapper's Drop
 * auto-rollbacks if neither `.commit()` nor `.rollback()` is called.
 */
interface ZeroshipTransaction {
  /** Commit the transaction. Idempotent if already settled. */
  commit(): Promise<void>;
  /** Rollback the transaction. Idempotent if already settled. */
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
interface ZeroshipMigration {
  status(): Promise<string>;
  cancel(): Promise<string>;
  reset(): Promise<string>;

  /**
   * Fetch the next batch of rows after `cursor`. Returns a JSON string
   * `{ rows: [...] }`. Each row is a plain object keyed by column name.
   */
  fetchBatch(cursor: number, batchSize: number): Promise<string>;

  /**
   * Commit one batch of per-row updates. `updatesJson` is a JSON array
   * of `{ id: number, set: { col: value, ... } }`. `deadLetterPksJson`
   * is a JSON array of row primary keys the SDK is skipping. If
   * `isDone=true`, drives the audit row to `terminalStatus` and
   * releases the advisory lock.
   */
  commitBatch(
    updatesJson: string,
    deadLetterPksJson: string,
    nextCursor: number,
    processedTotal: number,
    isDone: boolean,
    terminalStatus: string,
    errorMessage: string,
  ): Promise<string>;
}

/**
 * A live subscription wrapper minted by `env.db.openSubscription(name)`.
 * Synchronous to mint — calling it does not allocate any Postgres state;
 * the wrapper merely registers a slot in the per-isolate broker routing
 * table. The wrapper's GC finalizer is the safety-net release.
 *
 * `pollJson` resolves with one of:
 *
 * - `{"kind":"change", "op":"insert"|"update"|"delete",
 *    "collection":..., "pk": number|null, "columns": string[]}`
 * - `{"kind":"resync"}` — bounded queue overflowed; the client should
 *   re-fetch and discard cached results
 * - `{"kind":"closed"}` — subscription was closed; iterator terminates
 * - `null` — handle no longer exists (already closed and reaped)
 */
interface ZeroshipSubscription {
  pollJson(): Promise<string | null>;
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
  status(spec: { name: string; collection: string }): Promise<string>;
  cancel(spec: { name: string; collection: string }): Promise<string>;
  reset(spec: { name: string; collection: string }): Promise<string>;
}

// ---------------------------------------------------------------------------
// Db entry point — env.db
// ---------------------------------------------------------------------------

/**
 * The `zeroship.db` namespace surfaced as `env.db` on every isolate.
 * Owns the lifecycle of the pooled connection and mints typed wrappers
 * for collections, transactions, migration runs, and subscriptions.
 *
 * The flat per-collection CRUD methods from v1 were removed in 2026-05
 * — call `.collection(name)` first and use the returned wrapper.
 */
interface ZeroshipDb {
  // --- Schema ---

  /** Register a model — creates table and columns if not exist. */
  registerModel(collection: string, schema: ZeroshipDbSchema): Promise<void>;

  // --- Collection wrapper mint ---

  /**
   * Mint (or return the cached) Collection wrapper for `name`. Identity
   * is cached on the Db wrapper so repeated calls with the same name
   * return the same JS object — the SDK relies on this for per-name
   * lazy resolution.
   */
  collection(name: string): ZeroshipCollection;

  // --- Transactions ---

  /**
   * Begin a transaction. Returns a Transaction wrapper whose
   * `.commit()` / `.rollback()` are explicit methods. The wrapper's
   * Drop auto-rollbacks via connection close if neither is called.
   */
  beginTransaction(isolationLevel?: string): Promise<ZeroshipTransaction>;

  // --- Migration runs (B1) ---

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

  // --- Reactive subscriptions (C1 / P8a) ---

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

  // --- Replication operator surface ---

  /**
   * Operator-only: idempotently provision the per-app
   * Postgres publication + logical replication slot. Returns a
   * JSON `SetupOutcome` (`{publication, slot, created, confirmedFlushLsn}`).
   *
   * Requires `wal_level=logical` on the server.
   */
  replicationSetup(opts?: { appId?: string }): Promise<string>;

  /**
   * Operator-only: run the C1 watchdog query against
   * `pg_replication_slots`. Returns a JSON array of slot health
   * records `[{slot, active, restartLsn, confirmedFlushLsn, lagBytes, walStatus}]`.
   */
  replicationWatchdog(): Promise<string>;

  /**
   * Operator-only: drop replication slots that have been
   * inactive for at least `opts.inactiveSeconds` (default 3600).
   * Returns the names of dropped slots as a JSON array. Apps whose
   * slot was reaped see a `resync` event on next subscriber attach.
   */
  replicationDropAbandoned(opts?: { inactiveSeconds?: number }): Promise<string>;
}
