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
// Primitive value types at the native boundary (JSON-serializable)
// ---------------------------------------------------------------------------

/** Scalar value that can appear in a filter or document at the native boundary. */
type ZeroshipScalar = string | number | boolean | null;

// ---------------------------------------------------------------------------
// Database primitives (zeroship.db.*)
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

/** Find query options — key names must match what the Rust callback reads. */
interface ZeroshipDbFindOpts {
  limit?: number;
  offset?: number;
  orderBy?: Record<string, 1 | -1>;
  select?: string[];
}

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

/** Supported primitive field type names. */
type ZeroshipDbTypeName = "string" | "number" | "boolean" | "date" | "json" | "array";

/** Supported primitive item type names (for array fields). */
type ZeroshipDbPrimitiveTypeName = "string" | "number" | "boolean" | "date" | "json";

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
}

/** Normalized schema — field name → definition. */
type ZeroshipDbSchema = Record<string, ZeroshipDbFieldDef>;

// ---------------------------------------------------------------------------
// Native driver interface (zeroship.db)
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
