/**
 * Native bridge — resolves `env.db` for the migrations primitives.
 *
 * Mirrors `sdks/db/src/db.ts::getNativeDb()`. Callers may inject a
 * mock via the `native` option (used in tests).
 *
 * The `zeroship` virtual module is provided by the V8 runtime at run
 * time; importing it at the top level breaks Node-based unit tests
 * (which never reach the production resolver). We dynamic-import it
 * inside `getNativeMigrations()` so tests that pass a `nativeOverride`
 * never trigger the import.
 */

/**
 * Native Migration v8_class wrapper — minted by
 * `env.db.migrations.start(spec)` and used through this SDK's run loop.
 * Each method delegates to a `#[v8_async_method]` on the Rust-side
 * `Migration` struct (`crates/plugin-db/src/v8_classes/migration.rs`).
 *
 * The wrapper holds the (app_id, name, collection) triple internally
 * and its Weak finalizer auto-cancels if the wrapper is GC'd without
 * an explicit teardown.
 */
/**
 * Audit-row status snapshot returned by `status()`. Matches the Rust
 * `exec_status` JSON shape that the native layer resolves directly as
 * a JS object (no `JSON.parse` on the SDK side).
 */
export interface NativeStatus {
  exists: boolean;
  status: string | null;
  cursor: number;
  processed: number;
  deadLetterPks: number[];
  isDone: boolean;
  error: string | null;
}

/**
 * Per-row update payload inside the `updates` array of a
 * `commitBatch` spec. `id` is the row primary key; `set` is the field
 * map to write.
 */
export interface NativeCommitUpdate {
  id: number;
  set: Record<string, unknown>;
}

/**
 * Spec object handed to `commitBatch`. Walked directly from V8 on
 * the native side — no JSON.stringify on the SDK hot path.
 */
export interface NativeCommitSpec {
  updates: NativeCommitUpdate[];
  deadLetterPks: number[];
  nextCursor: number;
  processedTotal: number;
  isDone: boolean;
  terminalStatus?: string;
  errorMessage?: string;
}

export interface NativeMigration {
  status(): Promise<NativeStatus>;
  cancel(): Promise<void>;
  reset(): Promise<void>;
  fetchBatch(cursor: number, batchSize: number): Promise<Record<string, unknown>[]>;
  commitBatch(spec: NativeCommitSpec): Promise<void>;
}

/**
 * Native `Migrations` namespace surfaced as `env.db.migrations`. Two
 * concerns:
 *
 * - **Running** a migration: `start(spec)` mints a Migration wrapper;
 *   the SDK's loop drives `fetchBatch` / `commitBatch` on it; Drop
 *   auto-cancels if abandoned.
 *
 * - **Observing / controlling** an already-persisted migration row
 *   by coordinates: `status(spec)` / `cancel(spec)` / `reset(spec)`,
 *   each taking `{ name, collection }`. These read the audit table
 *   directly and don't acquire the advisory lock — safe to call
 *   while another worker has an active run.
 */
export interface NativeMigrations {
  start(spec: {
    name: string;
    collection: string;
    dryRun?: boolean;
    reset?: boolean;
  }): Promise<NativeMigration>;
  status(spec: { name: string; collection: string }): Promise<NativeStatus>;
  cancel(spec: { name: string; collection: string }): Promise<void>;
  reset(spec: { name: string; collection: string }): Promise<void>;
}

import { env } from "zeroship";

/**
 * Resolve the live native entry point from the per-isolate env.
 *
 * Static top-level import so the synthetic `zeroship` module resolves
 * at module-init time, not on first call. A dynamic `await
 * import("zeroship")` inside a mutation handler would trip the B3
 * capability gate ("mutation handlers cannot call fetch") because the
 * dev-bootstrap's module loader is fetch-based.
 */
let cachedNative: NativeMigrations | null = null;
export async function getNativeMigrations(): Promise<NativeMigrations> {
  if (cachedNative) return cachedNative;
  const dbAny = (env as { db?: { migrations?: NativeMigrations } } | undefined)?.db;
  const migrations = dbAny?.migrations;
  if (migrations && typeof migrations.start === "function"
      && typeof migrations.status === "function") {
    cachedNative = migrations;
    return migrations;
  }
  throw new Error(
    "@zeroship/migrations: env.db.migrations namespace not available — " +
      "runtime is missing the Migrations v8_class surface.",
  );
}

/**
 * Coerce a thrown value to an `Error` with a `.code` property. Native
 * v8_class rejections now attach `e.code` (and optionally `e.hint`)
 * directly on the JS exception — `OpError::coded` in Rust constructs a
 * plain Error and sets `code` / `hint` on it before the pump calls
 * `resolver.reject(...)`. The SDK reads `e.code` straight off the
 * Error; no more `JSON.parse(e.message)` envelope-fishing.
 */
export function toNativeError(e: unknown): Error & { code?: string; hint?: string } {
  if (e instanceof Error) return e as Error & { code?: string; hint?: string };
  return new Error(String(e));
}
