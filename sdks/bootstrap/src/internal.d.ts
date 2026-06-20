// Framework-internal type declarations for the DB platform capability
// handle (P9 PR 4 — §8). NOT part of the published `@zeroship/types`
// surface; creator code MUST NOT import `@zeroship/bootstrap`.
//
// These interfaces were moved here out of `@zeroship/types`'s
// `db.d.ts` when the platform-internal callables moved off `env.db` to
// the `__platform` capability handle. They describe the shape of the
// handle the runtime stashes on `env.db` under a V8 private symbol and
// hands to this package's `runtime-entry` via the
// `globalThis.__zsDbPlatform(db)` resolver. The `__platform` handle and
// these types are unreachable from creator JS:
//   - the handle lives in a private-symbol slot (invisible to
//     Object.keys / getOwnPropertyNames / getOwnPropertySymbols /
//     for..in / JSON, and not keyable from JS), and
//   - `env.db.__platform` (string access) is actively refused at
//     runtime with `PLATFORM_INTERNAL_ONLY`.
//
// This is an ambient `.d.ts` (no top-level import/export) so the
// `Zeroship*` interfaces are global, matching how they were declared in
// `db.d.ts` and so they can reference the ambient `ZeroshipDb*` schema
// types from `@zeroship/types` without re-importing them.

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
 * A live migration-run wrapper minted by
 * `__platform.migrations.start(spec)`. Holds the (app_id, name,
 * collection) triple plus the session-scoped Postgres advisory lock
 * that fences concurrent runs. The wrapper's Weak finalizer
 * auto-cancels if the wrapper is GC'd without an explicit terminal
 * `commitBatch(isDone=true, ...)`.
 *
 * Lifecycle: `start(spec)` mints the wrapper in the `active` state.
 * `commitBatch(isDone=true, ...)` / `cancel()` / `reset()` all drive it
 * to the `settled` state — subsequent `commitBatch` / `cancel` /
 * `reset` calls reject because the wrapper no longer holds the advisory
 * lock; `status()` continues to work (the audit row is still observable
 * by `(name, collection)` coordinates).
 */
interface ZeroshipMigration {
  status(): Promise<ZeroshipMigrationStatus>;
  cancel(): Promise<void>;
  reset(): Promise<void>;

  /**
   * Fetch the next batch of rows after `cursor`. Resolves with the row
   * array; each row is a plain object keyed by column name. `cursor` /
   * `batchSize` must be finite, integer-valued numbers in the `i64`
   * range; out-of-range values reject with a `RangeError`.
   */
  fetchBatch(cursor: number, batchSize: number): Promise<Record<string, unknown>[]>;

  /**
   * Commit one batch of per-row updates. The spec is walked from V8
   * directly — no `JSON.stringify` on the SDK side. If `isDone=true`,
   * drives the audit row to `terminalStatus` and releases the advisory
   * lock. Resolves void; rejects with the underlying Postgres error on
   * failure.
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

/**
 * The `Migrations` namespace surfaced as `__platform.migrations`. Two
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

/**
 * Operator-facing replication namespace, surfaced as
 * `__platform.replication`. Apps don't call these — the deploy
 * orchestrator / control plane does (via this package's runtime-entry).
 */
interface ZeroshipReplication {
  /**
   * Idempotently provision the per-app Postgres publication + logical
   * replication slot. Returns a JSON `SetupOutcome`
   * (`{publication, slot, created, confirmedFlushLsn}`). Requires
   * `wal_level=logical` on the server.
   */
  setup(opts?: { appId?: string }): Promise<string>;

  /**
   * Run the C1 watchdog query against `pg_replication_slots`. Returns a
   * JSON array of slot health records `[{slot, active, restartLsn,
   * confirmedFlushLsn, lagBytes, walStatus}]`.
   */
  watchdog(): Promise<string>;

  /**
   * Drop replication slots that have been inactive for at least
   * `opts.inactiveSeconds` (default 3600). Returns the names of dropped
   * slots as a JSON array. Apps whose slot was reaped see a `resync`
   * event on next subscriber attach.
   */
  dropAbandoned(opts?: { inactiveSeconds?: number }): Promise<string>;
}

/**
 * The platform-internal capability handle (P9 §8) — set on the native
 * `env.db` object under a V8 private symbol and reached only via the
 * runtime's `globalThis.__zsDbPlatform(db)` resolver. Holds the
 * callables that moved off `env.db` in P9 PR 4. Absent from the
 * published `@zeroship/types` surface; this is the framework-internal
 * shape `@zeroship/bootstrap`'s install path consumes.
 */
interface ZeroshipDbPlatform {
  /**
   * Register a model — creates table and columns if not exist. The
   * optional third argument carries named multi-column indexes declared
   * via `schema(...).index(name, fields)`; each materialises as a
   * CONCURRENTLY-built Postgres index named `"<collection>__<name>"`.
   */
  registerModel(
    collection: string,
    schema: ZeroshipDbSchema,
    indexes?: ZeroshipDbNamedIndex[],
    // H1 — the FULL declared-collection-name set (`Object.keys(schemas)`),
    // passed on every per-collection call so the dev SQLite drop pass can
    // tell a not-yet-registered sibling from a genuinely-removed collection.
    // Inert on PG. Optional for raw/older callers.
    declared?: readonly string[],
  ): Promise<void>;

  /**
   * **P5.5 PR 5** — install the per-app mask policy. Called once at app
   * boot from `defineMaskPolicy()`'s pending-slot drain.
   */
  setMaskPolicy(policy: Record<string, string[]>): Promise<Record<string, never>>;

  /**
   * Auto-spawn the supervised WAL consumer for this app. Idempotent.
   * Resolves once the publication + slot are durable. Any app-id-shaped
   * `opts` is ignored (the handle is bound to its own app at mint).
   */
  startReplicationConsumer(opts?: string): Promise<string>;

  /** Mint (or return the cached) Migrations namespace wrapper. */
  migrations: ZeroshipMigrations;

  /** Mint (or return the cached) Replication namespace wrapper. */
  replication: ZeroshipReplication;
}
