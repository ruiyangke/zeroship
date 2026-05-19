/**
 * Shared primitive types used across all zeroship native interfaces.
 */

/** Scalar value that can appear in a filter or document at the native boundary. */
type ZeroshipScalar = string | number | boolean | null;

/**
 * Postgres transaction isolation level. The canonical wire spelling is
 * lowercase and human-readable. Used by both `@zeroship/db`'s
 * `db.transaction({ isolationLevel })` and `@zeroship/server`'s
 * procedure `config.isolation` so the SDKs agree on the input shape
 * without crossing a package boundary at type-check time.
 *
 * `"read uncommitted"` is accepted as input (Postgres silently
 * upgrades it to `"read committed"`); higher levels trade throughput
 * for correctness — `serializable` may raise SQLSTATE 40001 on
 * commit, requiring an SDK retry.
 */
type ZeroshipIsolationLevel =
  | "read uncommitted"
  | "read committed"
  | "repeatable read"
  | "serializable";
