/**
 * Database primitives exposed through `env.db`.
 *
 * The runtime exposes `env.db` as a Db instance with a small set
 * of entry-point methods. Per-collection CRUD lives on the Collection
 * wrapper returned by `env.db.collection(name)`. Transactions and reactive
 * subscriptions use separate wrappers.
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
  | { $gt?: string | number | bigint }
  | { $gte?: string | number | bigint }
  | { $lt?: string | number | bigint }
  | { $lte?: string | number | bigint }
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

/** Values carried by the native database boundary. */
type ZeroshipDbValue =
  | ZeroshipScalar
  | Uint8Array
  | ZeroshipDbValue[]
  | { [key: string]: ZeroshipDbValue };

/** A single update field value — direct scalar or operator object. */
type ZeroshipDbUpdateValue =
  | ZeroshipDbValue
  | { $set?: ZeroshipDbValue }
  | { $inc?: number }
  | { $dec?: number }
  | { $mul?: number }
  | { $push?: ZeroshipDbValue }
  | { $pull?: ZeroshipDbValue }
  | { $addToSet?: ZeroshipDbValue };

// ---------------------------------------------------------------------------
// Query options
// ---------------------------------------------------------------------------

/** Find query options — key names must match what the Rust callback reads. */
interface ZeroshipDbFindOpts {
  /** Load declared forward references through the native ORM. */
  with?: Record<string, true>;
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
   * Include soft-deleted rows when the descriptor declares a soft-delete
   * assignment. Otherwise this option has no effect.
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
  | { $max: string };

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
  /** Execute a structured relational read prepared by the shared ORM. */
  read(query: Record<string, unknown>): Promise<Record<string, unknown>[]>;
  /** Find multiple documents. Returns the row array. */
  find(filter: ZeroshipDbFilter, opts?: ZeroshipDbFindOpts): Promise<Record<string, unknown>[]>;

  /** Insert one document. Returns the inserted row. */
  insert(doc: Record<string, ZeroshipDbValue>): Promise<Record<string, unknown>>;

  /** Insert multiple documents. Returns the inserted rows. */
  insertMany(docs: Record<string, ZeroshipDbValue>[]): Promise<Record<string, unknown>[]>;

  /** Update one document. Returns the updated row or `null` when nothing matched. */
  update(filter: ZeroshipDbFilter, update: ZeroshipDbUpdate): Promise<Record<string, unknown> | null>;

  /** Update multiple documents. Returns the count of affected rows. */
  updateMany(filter: ZeroshipDbFilter, update: ZeroshipDbUpdate): Promise<number>;

  /** Delete one document. Descriptor delete assignments determine whether the
   *  row is updated or physically deleted. */
  delete(filter: ZeroshipDbFilter): Promise<Record<string, unknown> | null>;

  /** Delete multiple documents. Returns the count of affected rows. */
  deleteMany(filter: ZeroshipDbFilter): Promise<number>;

  /** Physically delete one row regardless of descriptor delete assignments. */
  purge(filter: ZeroshipDbFilter): Promise<Record<string, unknown> | null>;

  /** bulk hard-delete. Returns the count of affected rows. */
  purgeMany(filter: ZeroshipDbFilter): Promise<number>;

  /** Restore one row through the descriptor's restore assignment. */
  restore(filter: ZeroshipDbFilter): Promise<Record<string, unknown> | null>;

  /** bulk-restore. Returns the count of restored rows. */
  restoreMany(filter: ZeroshipDbFilter): Promise<number>;

  /** Upsert a document (insert or update on conflict). Returns the row.
   *  `opts.conflictFields` names a non-empty application-owned unique key. */
  upsert(
    doc: Record<string, ZeroshipDbValue>,
    opts: { conflictFields: string[] },
  ): Promise<Record<string, unknown> | null>;

  /** Count documents matching `filter`. */
  count(
    filter: ZeroshipDbFilter,
    opts?: { include_deleted?: boolean },
  ): Promise<number>;

  /** Get distinct values for `opts.field` across rows matching `filter`. */
  distinct(
    filter: ZeroshipDbFilter,
    opts: { field: string; include_deleted?: boolean },
  ): Promise<(string | number | bigint | boolean | Uint8Array | null)[]>;

  /** Run an aggregation pipeline. */
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
  ): Promise<ZeroshipDbValue>;

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
  ): Promise<{ results: Record<string, Record<string, ZeroshipDbValue>> }>;
}

/**
 * The table view handed to an `env.db.transaction(fn)` callback.
 *
 * Each property is a tx-bound {@link ZeroshipCollection} — every CRUD op
 * routes through the open transaction connection automatically. There is
 * `collection(name)` reaches names that collide with view methods. The
 * transaction lifecycle is owned by the native Rust orchestrator: throw to
 * abort and resolve to commit.
 */
interface ZeroshipTxView {
  collection(name: string): ZeroshipCollection;
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
 * `env.db.<collection>.openSubscription()`.
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

// ---------------------------------------------------------------------------
// Db entry point — env.db
// ---------------------------------------------------------------------------

/**
 * The `zeroship.db` namespace surfaced as `env.db` on every isolate.
 * Native operations live on the Db instance. The host finalizes startup
 * configuration after creator evaluation.
 */
interface ZeroshipDb {
  /** Startup declaration used by @zeroship/db; native finalization seals it. */
  declareMaskPolicy(policy: Readonly<Record<string, readonly string[]>>): void;

  /**
   * Mint (or return the cached) Collection wrapper for `name`. Identity
   * is cached on the Db wrapper so repeated calls with the same name
   * return the same JS object — the SDK relies on this for per-name
   * lazy resolution.
   */
  collection(name: string): ZeroshipCollection;

  /**
   * Run `callback` inside a native transaction.
   *
   * `callback` receives a table-scoped {@link ZeroshipTxView}; the
   * returned promise resolves with the callback's result on **commit**
   * (callback resolved) and rejects with the callback's error on
   * **rollback** (callback threw / rejected). A `transaction(...)` call
   * made while a transaction is already active opens a `SAVEPOINT`.
   *
   * `opts.isolationLevel` is a {@link ZeroshipIsolationLevel} and applies to
   * the outermost `BEGIN`.
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

}
