/**
 * Database primitives (zeroship.db.*)
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
// Native driver interface
// ---------------------------------------------------------------------------

/** The zeroship.db namespace — all methods return JSON strings from the native layer. */
interface ZeroshipDb {
  // --- CRUD ---

  /** Find multiple documents. Returns JSON array string. */
  find(collection: string, filter: ZeroshipDbFilter, opts?: ZeroshipDbFindOpts): Promise<string>;

  /** Find one document. Returns JSON string or null. */
  findOne(collection: string, filter: ZeroshipDbFilter): Promise<string | null>;

  /** Insert one document. Returns JSON string of the inserted row. */
  insert(collection: string, doc: Record<string, ZeroshipScalar | ZeroshipScalar[]>): Promise<string>;

  /** Insert multiple documents. Returns JSON array string. */
  insertMany(collection: string, docs: Record<string, ZeroshipScalar | ZeroshipScalar[]>[]): Promise<string>;

  /** Update one document. Returns JSON string of the updated row or null. */
  updateOne(collection: string, filter: ZeroshipDbFilter, update: ZeroshipDbUpdate): Promise<string>;

  /** Update multiple documents. Returns JSON string with { updated: N }. */
  updateMany(collection: string, filter: ZeroshipDbFilter, update: ZeroshipDbUpdate): Promise<string>;

  /** Delete one document. Returns JSON string of the deleted row or null. */
  deleteOne(collection: string, filter: ZeroshipDbFilter): Promise<string>;

  /** Delete multiple documents. Returns JSON string with { deleted: N }. */
  deleteMany(collection: string, filter: ZeroshipDbFilter): Promise<string>;

  /** Upsert a document (insert or update on conflict). Returns JSON string of the row. */
  upsert(collection: string, doc: Record<string, ZeroshipScalar | ZeroshipScalar[]>, conflictFields: string[]): Promise<string>;

  /** Count documents matching filter. Returns JSON string with { count: N }. */
  count(collection: string, filter: ZeroshipDbFilter): Promise<string>;

  /** Get distinct values for a field. Returns JSON array string. */
  distinct(collection: string, field: string, filter: ZeroshipDbFilter): Promise<string>;

  /** Run an aggregation pipeline. Returns JSON array string. */
  aggregate(collection: string, pipeline: ZeroshipDbAggregateStage[]): Promise<string>;

  // --- Schema ---

  /** Register a model — creates table and columns if not exist. */
  registerModel(collection: string, schema: ZeroshipDbSchema): Promise<void>;

  // --- Transactions ---

  /** Begin a transaction. All subsequent CRUD ops use the same connection. */
  beginTransaction(isolationLevel?: string): Promise<void>;

  /** Commit the active transaction. */
  commitTransaction(): Promise<void>;

  /** Rollback the active transaction. */
  rollbackTransaction(): Promise<void>;

  // --- B1 — @zeroship/migrations primitives ---

  /**
   * Begin a data-backfill migration run. Acquires a session-scoped
   * Postgres advisory lock keyed by (app_id, name); subsequent calls
   * from other workers fail with `migration_already_running`.
   *
   * Returns a JSON string `{ auditId, cursor, processed, status, deadLetterPks }`.
   */
  migrationBegin(name: string, collection: string, dryRun: boolean, reset: boolean): Promise<string>;

  /**
   * Fetch the next batch of rows after `cursor`. Returns a JSON string
   * `{ rows: [...] }`. Each row is a plain object keyed by column name.
   */
  migrationFetchBatch(cursor: number, batchSize: number): Promise<string>;

  /**
   * Commit one batch of per-row updates. `updatesJson` is a JSON array
   * of `{ id: number, set: { col: value, ... } }`. `deadLetterPksJson`
   * is a JSON array of row primary keys the SDK is skipping. If
   * `isDone=true`, drives the audit row to `terminalStatus` and
   * releases the advisory lock.
   */
  migrationCommitBatch(
    updatesJson: string,
    deadLetterPksJson: string,
    nextCursor: number,
    processedTotal: number,
    isDone: boolean,
    terminalStatus: string,
    errorMessage: string,
  ): Promise<string>;

  /** Read the current audit-row state for a (collection, name) pair. */
  migrationStatus(name: string, collection: string): Promise<string>;

  /**
   * Cancel a `pending` or `running` migration. Subsequent
   * `migrationFetchBatch` calls return `migration_cancelled`.
   */
  migrationCancel(name: string, collection: string): Promise<string>;

  /**
   * Reset a migration's persisted state (status → pending, cursor → 0,
   * dead_letter_pks → null). Used after a `cancelled` or `failed` run.
   */
  migrationReset(name: string, collection: string): Promise<string>;

  // --- C1 / P8a — reactive queries (in-process broker) ---

  /**
   * Open a subscription on `collection`. Returns a numeric handle.
   * Synchronous — calling it does not allocate any Postgres state;
   * the handle merely registers a slot in the per-isolate broker
   * routing table. Pair with [`subscribePoll`] and [`subscribeClose`].
   *
   * Today the events come from local mutations in the same isolate
   * (coarse-grained, every change to `collection`). The proposal's
   * read-set narrowing + cross-worker WAL pickup are P8b / P8a.2.
   */
  subscribe(collection: string): number;

  /**
   * Await the next pending event for `handle`. The promise stays
   * pending until an event arrives or the subscription is closed.
   *
   * Resolved value is one of:
   *
   * - `{"kind":"change", "op":"insert"|"update"|"delete",
   *    "collection":..., "pk": number|null, "columns": string[]}`
   * - `{"kind":"resync"}` — bounded queue overflowed; the client
   *   should re-fetch and discard cached results
   * - `{"kind":"closed"}` — subscription was closed; iterator
   *   should terminate
   * - `null` — handle no longer exists (already closed and reaped)
   */
  subscribePoll(handle: number): Promise<string | null>;

  /** Close `handle`. Any pending poll resolves with `{"kind":"closed"}`. */
  subscribeClose(handle: number): void;

  /**
   * Operator-only: idempotently provision the per-app
   * Postgres publication + logical replication slot. Returns a
   * JSON `SetupOutcome` (`{publication, slot, created, confirmedFlushLsn}`).
   *
   * Requires `wal_level=logical` on the server.
   */
  replicationSetup(appId?: string): Promise<string>;

  /**
   * Operator-only: run the C1 watchdog query against
   * `pg_replication_slots`. Returns a JSON array of slot health
   * records `[{slot, active, restartLsn, confirmedFlushLsn, lagBytes, walStatus}]`.
   */
  replicationWatchdog(): Promise<string>;

  /**
   * Operator-only: drop replication slots that have been
   * inactive for at least `inactiveSeconds`. Returns the names of
   * dropped slots as a JSON array. Apps whose slot was reaped see a
   * `resync` event on next subscriber attach.
   */
  replicationDropAbandoned(inactiveSeconds?: number): Promise<string>;
}
