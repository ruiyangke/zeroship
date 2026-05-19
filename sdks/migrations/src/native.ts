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
 * Subset of `ZeroshipDb` the migrations SDK calls. We pull only the
 * `migration*` methods so test mocks stay small.
 */
export interface NativeMigrations {
  migrationBegin(name: string, collection: string, dryRun: boolean, reset: boolean): Promise<string>;
  migrationFetchBatch(cursor: number, batchSize: number): Promise<string>;
  migrationCommitBatch(
    updatesJson: string,
    deadLetterPksJson: string,
    nextCursor: number,
    processedTotal: number,
    isDone: boolean,
    terminalStatus: string,
    errorMessage: string,
  ): Promise<string>;
  migrationStatus(name: string, collection: string): Promise<string>;
  migrationCancel(name: string, collection: string): Promise<string>;
  migrationReset(name: string, collection: string): Promise<string>;
}

import { env } from "zeroship";

/**
 * Resolve the live native namespace from the per-isolate env.
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
  const db = (env as { db?: NativeMigrations } | undefined)?.db;
  if (db && typeof db.migrationBegin === "function") {
    cachedNative = db;
    return db;
  }
  throw new Error(
    "@zeroship/migrations: env.db.migration* not available — " +
      "is the DbPlugin registered on this runtime?",
  );
}

/**
 * Parse a JSON string returned by the native layer. Native ops resolve
 * with `{ error: "..." }` instead of rejecting on structured failures
 * (matches @zeroship/db's pattern).
 */
export function parseNative<T>(raw: string): T {
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch (e) {
    throw new Error(`@zeroship/migrations: failed to parse native response: ${e instanceof Error ? e.message : String(e)}`);
  }
  if (parsed && typeof parsed === "object" && "error" in parsed) {
    const errVal = (parsed as { error: unknown }).error;
    if (typeof errVal === "string") throw new Error(errVal);
    if (errVal && typeof errVal === "object" && "code" in errVal) {
      const code = String((errVal as { code: unknown }).code);
      const msg = "message" in errVal ? String((errVal as { message: unknown }).message) : code;
      const e = new Error(msg) as Error & { code: string };
      e.code = code;
      throw e;
    }
  }
  return parsed as T;
}

/**
 * Some native ops throw structured `{ code, message }` strings via the
 * promise reject path. Convert them to Error instances with `.code`.
 */
export function toNativeError(e: unknown): Error & { code?: string } {
  if (e instanceof Error) {
    // Try to parse a JSON envelope from the message.
    try {
      const parsed = JSON.parse(e.message);
      if (parsed && typeof parsed === "object" && "code" in parsed) {
        const code = String(parsed.code);
        const msg = "message" in parsed ? String(parsed.message) : code;
        const wrapped = new Error(msg) as Error & { code: string };
        wrapped.code = code;
        return wrapped;
      }
    } catch {
      // not JSON — leave as-is
    }
    return e;
  }
  return new Error(String(e));
}
