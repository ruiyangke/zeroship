// @zeroship/kv — key-value SDK.
//
// Typed wrapper around the native `env.kv` v8_class surface. Stores
// JSON-serializable values with optional TTL.
//
//   env.kv is for ephemeral, hot-path, expiring data.
//   env.db is for anything that is a source of truth.
//
// Most KV scenarios are *implementable* on env.db (also strongly
// consistent). KV earns its place for: not polluting the relational
// store with high-frequency ephemeral churn (rate-limit counters, page
// views, cache fills); TTL as a first-class feature; offloading the
// source-of-truth DB. Durable idempotency / locks / config / tokens
// belong in env.db. See docs/reference/kv.md for the full guidance.
//
// Backends (selected by the runtime, identical wire contract): InMemory
// (dev/test), redb (single-process persistent, ZEROSHIP_KV_PATH), Redis
// (distributed, ZEROSHIP_KV_URL).

import { env } from "zeroship";

/**
 * The native `env.kv` surface registered by the Rust `KvPlugin`. Every
 * method returns a Promise; validation failures (bad key/value/args)
 * throw synchronously as `TypeError`s before the Promise is created.
 *
 * Wire contract (verified against `crates/plugin-kv/src/dispatch.rs`):
 * - `get` resolves the raw stored string, or JS `null` if missing — it
 *   does NOT JSON-wrap, so a single `JSON.parse` round-trips a value the
 *   SDK stored via `JSON.stringify`.
 * - `set` resolves `{ ok: true }` (ignored by the SDK).
 * - `incr` resolves a JS `number`, or a `bigint` when `|n| > 2^53`.
 * - `ttl` resolves `null` for a missing key, `{ ttlMs: null }` for an
 *   existing key with no expiry, `{ ttlMs: <ms> }` otherwise.
 */
interface NativeKv {
  get(key: string): Promise<string | null>;
  set(key: string, value: string, opts?: { ttlMs?: number }): Promise<{ ok: true }>;
  delete(key: string): Promise<{ deleted: boolean }>;
  incr(key: string, opts?: { by?: number; ttlMs?: number }): Promise<number | bigint>;
  setIfAbsent(
    key: string,
    value: string,
    opts?: { ttlMs?: number },
  ): Promise<{ stored: boolean }>;
  expire(key: string, ttlMs: number): Promise<{ updated: boolean }>;
  ttl(key: string): Promise<{ ttlMs: number | null } | null>;
  persist(key: string): Promise<{ updated: boolean }>;
  list(
    prefix?: string,
    opts?: { cursor?: string; limit?: number },
  ): Promise<{ keys: string[]; cursor: string | null }>;
}

function getNativeKv(): NativeKv {
  const k = (env as { kv?: NativeKv } | undefined)?.kv;
  if (!k) {
    throw new Error(
      "@zeroship/kv: env.kv not available — is the KvPlugin registered on this runtime?",
    );
  }
  return k;
}

export type Result<T> = { data: T; error: null } | { data: null; error: Error };
const ok = <T>(d: T): Result<T> => ({ data: d, error: null });
const err = <T>(e: unknown): Result<T> => ({
  data: null,
  error: e instanceof Error ? e : new Error(String(e)),
});

export interface SetOptions {
  /** TTL in milliseconds. The entry is deleted once this elapses. */
  ttlMs?: number;
}

export interface IncrOptions {
  /** Amount to add (may be negative). Defaults to `1`. */
  by?: number;
  /**
   * TTL in milliseconds applied **only when this call creates the key**
   * — an existing key keeps its current expiry. This is the fixed-window
   * rate-limit shape: `incr(k, { by: 1, ttlMs: 60_000 })`.
   */
  ttlMs?: number;
}

export interface ListOptions {
  /** Opaque, backend-specific continuation cursor. `null`/omit = start. */
  cursor?: string;
  /** Page size. Defaults to 1000; clamped to 10 000 by the runtime. */
  limit?: number;
}

export interface ListResult {
  keys: string[];
  /** Opaque continuation cursor; `null` means the listing is exhausted. */
  cursor: string | null;
}

/**
 * Typed key-value client.
 *
 * ```ts
 * import { kv } from "@zeroship/kv";
 * await kv.set("user:42:session", { token: "…" }, { ttlMs: 3600_000 });
 * const { data: session } = await kv.get<{ token: string }>("user:42:session");
 * ```
 *
 * Every method returns a `Result<T>` (`{ data, error }`). On a thrown
 * native error (`kv_non_numeric`, `kv_overflow`, …) `error` is non-null
 * and carries the `.code`; branch on `error.code`, never on the message.
 */
class Kv {
  /**
   * Native handle. When constructed without an override (the default
   * `kv` singleton) it resolves lazily on first use via [`getNativeKv`],
   * so `import { kv }` never throws at module-load time in an
   * environment without `env.kv` (tests, tooling) — the error surfaces
   * only when a method is actually called. `protected` so `namespace()`
   * can wrap the resolved handle.
   */
  protected get native(): NativeKv {
    this.#native ??= getNativeKv();
    return this.#native;
  }
  #native: NativeKv | undefined;

  constructor(nativeOverride?: NativeKv) {
    this.#native = nativeOverride;
  }

  // --- Core surface (1:1 with the native v8_class) ---------------------

  /**
   * Retrieve a value, JSON-decoded. Returns `null` (wrapped in `ok()`)
   * when the key is missing or its TTL has elapsed.
   */
  async get<T = unknown>(key: string): Promise<Result<T | null>> {
    try {
      const raw = await this.native.get(key);
      if (raw === null) return ok(null);
      // The native layer hands back the raw stored string verbatim; we
      // stored `JSON.stringify(value)`, so one parse restores the value.
      return ok(JSON.parse(raw) as T);
    } catch (e) {
      return err(e);
    }
  }

  /**
   * Convenience: read a value that was written as a plain string. Like
   * `get`, but returns the decoded string directly (the stored form is
   * still JSON, so this unwraps the JSON-encoded string).
   */
  async getString(key: string): Promise<Result<string | null>> {
    try {
      const raw = await this.native.get(key);
      if (raw === null) return ok(null);
      return ok(JSON.parse(raw) as string);
    } catch (e) {
      return err(e);
    }
  }

  /** Store a value (JSON-encoded). Pass `{ ttlMs }` to expire it. */
  async set<T>(key: string, value: T, opts: SetOptions = {}): Promise<Result<void>> {
    try {
      const str = JSON.stringify(value);
      await this.native.set(key, str, opts);
      return ok(undefined);
    } catch (e) {
      return err(e);
    }
  }

  /** Delete a key. `deleted` is `false` if the key was already absent. */
  async delete(key: string): Promise<Result<{ deleted: boolean }>> {
    try {
      return ok(await this.native.delete(key));
    } catch (e) {
      return err(e);
    }
  }

  /**
   * Atomically add to a counter and return the new value. `opts.by`
   * defaults to `+1`; `opts.ttlMs` sets the expiry **only when this call
   * creates the key** (an existing counter keeps its TTL).
   *
   * Counters are exact up to `Number.MAX_SAFE_INTEGER` (2^53). Above
   * that the native layer resolves a `bigint`; this method normalizes it
   * back to `number` with `Number(...)`, so precision degrades past
   * 2^53. That range is not a KV-counter use case — use `env.db` for
   * exact large-integer accumulation.
   *
   * Rejects with `error.code === "kv_non_numeric"` if the existing value
   * isn't an integer, or `"kv_overflow"` on i64 over/underflow.
   */
  async incr(key: string, opts: IncrOptions = {}): Promise<Result<number>> {
    try {
      const n = await this.native.incr(key, opts);
      return ok(typeof n === "bigint" ? Number(n) : n);
    } catch (e) {
      return err(e);
    }
  }

  /**
   * Atomically store a value only if the key is absent. `stored` is
   * `false` when the key already existed.
   *
   * Use this for **ephemeral** locks / idempotency that auto-release via
   * `ttlMs` (e.g. a 30s "this job is being processed" flag). For durable
   * "process this exactly once" semantics, use `env.db` with a unique
   * index — a KV entry can expire or be evicted.
   */
  async setIfAbsent<T>(
    key: string,
    value: T,
    opts: SetOptions = {},
  ): Promise<Result<{ stored: boolean }>> {
    try {
      const str = JSON.stringify(value);
      return ok(await this.native.setIfAbsent(key, str, opts));
    } catch (e) {
      return err(e);
    }
  }

  /** Set/replace a key's TTL. `updated` is `false` if the key is absent. */
  async expire(key: string, ttlMs: number): Promise<Result<{ updated: boolean }>> {
    try {
      return ok(await this.native.expire(key, ttlMs));
    } catch (e) {
      return err(e);
    }
  }

  /**
   * Inspect a key's TTL:
   * - `{ ttlMs: null }` for an existing key with no expiry,
   * - `{ ttlMs: number }` for the remaining milliseconds, or
   * - `null` when the key does not exist.
   */
  async ttl(key: string): Promise<Result<{ ttlMs: number | null } | null>> {
    try {
      return ok(await this.native.ttl(key));
    } catch (e) {
      return err(e);
    }
  }

  /** Remove a key's TTL (make it permanent). `updated` is `false` if the key has no TTL or is absent. */
  async persist(key: string): Promise<Result<{ updated: boolean }>> {
    try {
      return ok(await this.native.persist(key));
    } catch (e) {
      return err(e);
    }
  }

  /**
   * List keys under an optional literal `prefix` (no glob), paginated.
   * Returns `{ keys, cursor }`; pass the returned `cursor` back to fetch
   * the next page. The cursor is opaque and backend-specific — don't
   * parse it. `cursor === null` means the listing is exhausted.
   *
   * ```ts
   * let cursor: string | null = null;
   * do {
   *   const { data } = await kv.list("session:", { cursor: cursor ?? undefined });
   *   for (const k of data!.keys) { ... }
   *   cursor = data!.cursor;
   * } while (cursor !== null);
   * ```
   */
  async list(prefix = "", opts: ListOptions = {}): Promise<Result<ListResult>> {
    try {
      return ok(await this.native.list(prefix, opts));
    } catch (e) {
      return err(e);
    }
  }

  // --- SDK-only conveniences (pure JS over the kernel) -----------------

  /** `true` if the key exists (and hasn't expired). */
  async has(key: string): Promise<Result<boolean>> {
    const r = await this.get(key);
    if (r.error) return err(r.error);
    return ok(r.data !== null);
  }

  /**
   * Cache-aside read-through: return the stored value, or compute it via
   * `factory`, store it (with optional `ttlMs`), and return it.
   *
   * **Not atomic.** Under a cache miss, concurrent callers may each run
   * `factory` (a "stampede") — acceptable for caching, since the last
   * `set` wins and all callers get a valid value. If you need
   * exactly-once computation, use `setIfAbsent` as a lock.
   */
  async getOrSet<T>(
    key: string,
    opts: SetOptions,
    factory: () => T | Promise<T>,
  ): Promise<Result<T>> {
    try {
      const existing = await this.get<T>(key);
      if (existing.error) return err(existing.error);
      if (existing.data !== null) return ok(existing.data);
      const value = await factory();
      const set = await this.set(key, value, opts);
      if (set.error) return err(set.error);
      return ok(value);
    } catch (e) {
      return err(e);
    }
  }

  /**
   * Return a sub-client that transparently prepends `prefix` to every
   * key. Pure key-prefixing sugar (string concat) — no native namespace
   * concept exists; `list()` queries the combined prefix.
   *
   * ```ts
   * const sessions = kv.namespace("session:");
   * await sessions.set("abc", data);   // writes "session:abc"
   * ```
   */
  namespace(prefix: string): Kv {
    // Build a NativeKv shim that prepends `prefix` and forwards to this
    // client's native handle, then wrap it in a fresh Kv so all of Kv's
    // logic (JSON round-trip, bigint normalization, conveniences) is
    // reused verbatim. Namespaces compose — `ns("a:").namespace("b:")`
    // chains the prefixes.
    const native = this.native;
    return new Kv({
      get: (k) => native.get(prefix + k),
      set: (k, v, o) => native.set(prefix + k, v, o),
      delete: (k) => native.delete(prefix + k),
      incr: (k, o) => native.incr(prefix + k, o),
      setIfAbsent: (k, v, o) => native.setIfAbsent(prefix + k, v, o),
      expire: (k, t) => native.expire(prefix + k, t),
      ttl: (k) => native.ttl(prefix + k),
      persist: (k) => native.persist(prefix + k),
      // list prefixes the *query* prefix; returned keys keep their full
      // (prefixed) form, matching the underlying store.
      list: (p = "", o) => native.list(prefix + p, o),
    });
  }
}

/** Default singleton — most apps just import `kv` and go. */
export const kv = new Kv();

/** Factory if you need to pass a mock native (for tests). */
export function createKv(nativeOverride?: NativeKv): Kv {
  return new Kv(nativeOverride);
}
