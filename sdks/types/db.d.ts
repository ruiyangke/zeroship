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

/** Supported primitive field type names. Includes `"ref"` for B2 typed FKs. */
type ZeroshipDbTypeName = "string" | "number" | "boolean" | "date" | "json" | "array" | "ref";

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
}
