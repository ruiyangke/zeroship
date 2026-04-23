// @zeroship/kv — key-value SDK.
//
// Typed wrapper around the `zeroship.kv.*` native primitives. Stores
// JSON-serializable values with optional TTL. In dev the backend is an
// in-memory map per-worker; in prod it'll be Redis/Upstash behind the
// same native interface.

import { env } from "zeroship";

interface NativeKv {
  get(key: string): Promise<string>;             // JSON-encoded string | "null"
  set(key: string, value: string, ttlMs?: number): Promise<string>;
  delete(key: string): Promise<string>;
  incr(key: string, delta?: number): Promise<string>;  // numeric string
  list(prefix?: string): Promise<string>;        // JSON array of strings
}

function getNativeKv(): NativeKv {
  const k = (env as { kv?: NativeKv } | undefined)?.kv;
  if (!k) {
    throw new Error(
      "@zeroship/kv: env.kv not available — is the KvPlugin registered on this runtime?"
    );
  }
  return k;
}

export type Result<T> = { data: T; error: null } | { data: null; error: Error };
const ok = <T>(d: T): Result<T> => ({ data: d, error: null });
const err = <T>(e: Error): Result<T> => ({ data: null, error: e });

export interface SetOptions {
  /** TTL in milliseconds. Entry is deleted on access after this expires. */
  ttlMs?: number;
}

/**
 * Typed key-value client.
 *
 * ```ts
 * import { kv } from "@zeroship/kv";
 * await kv.set("user:42:session", { token: "…" }, { ttlMs: 3600_000 });
 * const session = await kv.get<{ token: string }>("user:42:session");
 * ```
 */
class Kv {
  readonly #native: NativeKv;
  constructor(nativeOverride?: NativeKv) {
    this.#native = nativeOverride ?? getNativeKv();
  }

  /** Retrieve a value. Returns `null` (wrapped in ok()) if missing. */
  async get<T = unknown>(key: string): Promise<Result<T | null>> {
    try {
      const raw = await this.#native.get(key);
      if (raw === "null" || raw === null) return ok(null);
      const envelope = JSON.parse(raw) as string;
      // Native layer wraps the stored string in JSON; we double-parse
      // to get the user's original structured value back.
      return ok(JSON.parse(envelope) as T);
    } catch (e) {
      return err(e instanceof Error ? e : new Error(String(e)));
    }
  }

  /** Convenience: get as raw string without the extra JSON unwrap. */
  async getString(key: string): Promise<Result<string | null>> {
    try {
      const raw = await this.#native.get(key);
      if (raw === "null" || raw === null) return ok(null);
      return ok(JSON.parse(raw) as string);
    } catch (e) {
      return err(e instanceof Error ? e : new Error(String(e)));
    }
  }

  /** Store a value. Serialized via JSON.stringify — pass any plain object. */
  async set<T>(key: string, value: T, opts: SetOptions = {}): Promise<Result<void>> {
    try {
      const str = JSON.stringify(value);
      await this.#native.set(key, str, opts.ttlMs);
      return ok(undefined);
    } catch (e) {
      return err(e instanceof Error ? e : new Error(String(e)));
    }
  }

  async delete(key: string): Promise<Result<{ deleted: boolean }>> {
    try {
      const raw = await this.#native.delete(key);
      return ok(JSON.parse(raw) as { deleted: boolean });
    } catch (e) {
      return err(e instanceof Error ? e : new Error(String(e)));
    }
  }

  /** Atomic counter. Returns the new value. `delta` defaults to +1. */
  async incr(key: string, delta = 1): Promise<Result<number>> {
    try {
      const raw = await this.#native.incr(key, delta);
      return ok(Number(raw));
    } catch (e) {
      return err(e instanceof Error ? e : new Error(String(e)));
    }
  }

  /** List keys matching an optional prefix. */
  async list(prefix = ""): Promise<Result<string[]>> {
    try {
      const raw = await this.#native.list(prefix);
      return ok(JSON.parse(raw) as string[]);
    } catch (e) {
      return err(e instanceof Error ? e : new Error(String(e)));
    }
  }
}

/** Default singleton — most apps just import `kv` and go. */
export const kv = new Kv();

/** Factory if you need to pass a mock native (for tests). */
export function createKv(nativeOverride?: NativeKv): Kv { return new Kv(nativeOverride); }
