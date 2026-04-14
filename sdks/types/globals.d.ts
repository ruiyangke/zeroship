/**
 * @zeroship/types — TypeScript declarations for the zeroship runtime.
 *
 * These types describe the `zeroship` global object available inside
 * V8 isolates. Install as a dev dependency for autocomplete and type
 * checking when calling native primitives directly.
 *
 * Usage:
 *   npm install -D @zeroship/types
 *   // tsconfig.json: { "types": ["@zeroship/types"] }
 *
 * Or simply reference the file:
 *   /// <reference types="@zeroship/types" />
 */

// ---------------------------------------------------------------------------
// Database primitives (zeroship.db.*)
// ---------------------------------------------------------------------------

/** Filter object — MongoDB-style query operators */
interface ZeroshipDbFilter {
  [field: string]:
    | unknown
    | { $eq?: unknown }
    | { $ne?: unknown }
    | { $gt?: unknown }
    | { $gte?: unknown }
    | { $lt?: unknown }
    | { $lte?: unknown }
    | { $in?: unknown[] }
    | { $nin?: unknown[] }
    | { $like?: string }
    | { $ilike?: string }
    | { $search?: string }
    | { $exists?: boolean };
}

/** Update object — per-field operators */
interface ZeroshipDbUpdate {
  [field: string]:
    | unknown
    | { $set?: unknown }
    | { $inc?: number }
    | { $dec?: number }
    | { $mul?: number }
    | { $push?: unknown }
    | { $pull?: unknown }
    | { $addToSet?: unknown };
}

/** Find query options */
interface ZeroshipDbFindOpts {
  limit?: number;
  offset?: number;
  orderBy?: Record<string, 1 | -1>;
  select?: string[];
}

/** Aggregate pipeline stage */
type ZeroshipDbAggregateStage =
  | { $match: ZeroshipDbFilter }
  | { $group: { by?: string | string[]; [agg: string]: unknown } }
  | { $having: ZeroshipDbFilter }
  | { $sort: Record<string, 1 | -1> }
  | { $limit: number };

/** Normalized schema field definition */
interface ZeroshipDbFieldDef {
  type: string;
  required?: boolean;
  unique?: boolean;
  index?: boolean;
  default?: unknown;
  min?: number;
  max?: number;
  enum?: string[];
  pattern?: RegExp;
  items?: string;
}

/** Normalized schema — field name → definition */
type ZeroshipDbSchema = Record<string, ZeroshipDbFieldDef>;

/** The zeroship.db namespace */
interface ZeroshipDb {
  // --- CRUD ---

  /** Find multiple documents. Returns JSON array string. */
  find(collection: string, filter: ZeroshipDbFilter, opts?: ZeroshipDbFindOpts): Promise<string>;

  /** Find one document. Returns JSON string or null. */
  findOne(collection: string, filter: ZeroshipDbFilter): Promise<string | null>;

  /** Insert one document. Returns JSON string of the inserted row. */
  insert(collection: string, doc: Record<string, unknown>): Promise<string>;

  /** Insert multiple documents. Returns JSON array string. */
  insertMany(collection: string, docs: Record<string, unknown>[]): Promise<string>;

  /** Update one document. Returns JSON string of the updated row or null. */
  updateOne(collection: string, filter: ZeroshipDbFilter, update: ZeroshipDbUpdate): Promise<string>;

  /** Update multiple documents. Returns JSON string with { updated: N }. */
  updateMany(collection: string, filter: ZeroshipDbFilter, update: ZeroshipDbUpdate): Promise<string>;

  /** Delete one document. Returns JSON string of the deleted row or null. */
  deleteOne(collection: string, filter: ZeroshipDbFilter): Promise<string>;

  /** Delete multiple documents. Returns JSON string with { deleted: N }. */
  deleteMany(collection: string, filter: ZeroshipDbFilter): Promise<string>;

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
  beginTransaction(): Promise<void>;

  /** Commit the active transaction. */
  commitTransaction(): Promise<void>;

  /** Rollback the active transaction. */
  rollbackTransaction(): Promise<void>;
}

// ---------------------------------------------------------------------------
// Global namespace
// ---------------------------------------------------------------------------

/** The zeroship runtime global — available in V8 isolates. */
interface ZeroshipGlobal {
  db: ZeroshipDb;
}

declare var zeroship: ZeroshipGlobal;

declare namespace globalThis {
  var zeroship: ZeroshipGlobal;
}
