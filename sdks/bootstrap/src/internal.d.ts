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
 * Operator-facing replication namespace, surfaced as
 * `__platform.replication`. Apps don't call these — the deploy
 * orchestrator / control plane does (via this package's runtime-entry).
 */
interface ZeroshipReplication {
  /**
   * Run the C1 watchdog query against `pg_replication_slots`. Returns a
   * JSON array of slot health records `[{slot, active, restartLsn,
   * confirmedFlushLsn, lagBytes, walStatus}]`.
   */
  watchdog(): Promise<string>;
}

/**
 * The platform-internal capability handle (P9 §8) — set on the native
 * `env.db` object under a V8 private symbol and reached only via the
 * runtime's `globalThis.__zsDbPlatform(db)` resolver. Holds the
 * remaining platform-only callables. Absent from the published
 * `@zeroship/types` surface; this is the framework-internal shape the
 * bootstrap mask-policy path consumes.
 */
interface ZeroshipDbPlatform {
  /**
   * **P5.5 PR 5** — install the per-app mask policy. Called once at app
   * boot from `defineMaskPolicy()`'s pending-slot drain.
   */
  setMaskPolicy(policy: Record<string, string[]>): Promise<Record<string, never>>;

  /** Mint (or return the cached) Replication namespace wrapper. */
  replication: ZeroshipReplication;
}
