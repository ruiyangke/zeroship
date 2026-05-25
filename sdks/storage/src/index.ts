// @zeroship/storage — object-storage SDK.
//
// Wraps the `zeroship.storage.*` native primitives in a typed, ergonomic
// Bucket interface. Same shape your app will use in dev (local runtime state
// local state plus local FS) and prod (S3/R2 once those land behind the
// same native interface).

import { env } from "zeroship";

// ---------------------------------------------------------------------------
// Native primitive shape — what the DbPlugin / StoragePlugin actually exposes
// on env.storage. All inputs/outputs are JSON strings; this SDK handles the
// (de)serialization and base64 plumbing so user code works in native types.
// ---------------------------------------------------------------------------

interface NativeStorage {
  put(bucket: string, key: string, bytesBase64: string, contentType?: string): Promise<string>;
  get(bucket: string, key: string): Promise<string>;
  delete(bucket: string, key: string): Promise<string>;
  list(bucket: string, prefix?: string): Promise<string>;
}

function getNativeStorage(): NativeStorage {
  const s = (env as { storage?: NativeStorage } | undefined)?.storage;
  if (!s) {
    throw new Error(
      "@zeroship/storage: env.storage not available — " +
      "is the StoragePlugin registered on this runtime?"
    );
  }
  return s;
}

// ---------------------------------------------------------------------------
// Result envelope — matches the db SDK's { data, error } shape so callers can
// branch uniformly without wrapping every call in try/catch.
// ---------------------------------------------------------------------------

export type Result<T> = { data: T; error: null } | { data: null; error: Error };

function ok<T>(data: T): Result<T> { return { data, error: null }; }
function err<T>(e: Error): Result<T> { return { data: null, error: e }; }

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

export interface PutResult {
  bucket: string;
  key: string;
  size: number;
}

export interface GetResult {
  /** Raw bytes as a Uint8Array. */
  bytes: Uint8Array;
  /** Server-recorded content-type (may be null until we add sidecar metadata). */
  contentType: string | null;
  size: number;
}

export interface ListEntry {
  key: string;
  size: number;
  modifiedAt: Date;
}

// ---------------------------------------------------------------------------
// Bucket — the primary API
// ---------------------------------------------------------------------------

export class Bucket {
  readonly #native: NativeStorage;
  readonly #name: string;

  constructor(name: string, nativeOverride?: NativeStorage) {
    if (!name) throw new Error("Bucket name must be non-empty");
    this.#name = name;
    this.#native = nativeOverride ?? getNativeStorage();
  }

  /** Store bytes at `key`. Accepts Uint8Array, ArrayBuffer, string, Blob. */
  async put(
    key: string,
    body: Uint8Array | ArrayBuffer | string | Blob,
    opts: { contentType?: string } = {},
  ): Promise<Result<PutResult>> {
    try {
      const bytes = await toBytes(body);
      const b64 = bytesToBase64(bytes);
      const raw = await this.#native.put(this.#name, key, b64, opts.contentType);
      const parsed = JSON.parse(raw) as PutResult;
      return ok(parsed);
    } catch (e) {
      return err(e instanceof Error ? e : new Error(String(e)));
    }
  }

  /** Fetch an object. Returns `{data: null}` if the key doesn't exist. */
  async get(key: string): Promise<Result<GetResult | null>> {
    try {
      const raw = await this.#native.get(this.#name, key);
      if (raw === "null" || raw === null) return ok(null);
      const parsed = JSON.parse(raw) as {
        bytesBase64: string;
        contentType: string | null;
        size: number;
      };
      return ok({
        bytes: base64ToBytes(parsed.bytesBase64),
        contentType: parsed.contentType,
        size: parsed.size,
      });
    } catch (e) {
      return err(e instanceof Error ? e : new Error(String(e)));
    }
  }

  /** Convenience: fetch an object and return its bytes as a UTF-8 string. */
  async getText(key: string): Promise<Result<string | null>> {
    const r = await this.get(key);
    if (r.error) return err(r.error);
    if (!r.data) return ok(null);
    return ok(new TextDecoder().decode(r.data.bytes));
  }

  /** Remove an object. `{deleted: false}` if it wasn't there. */
  async delete(key: string): Promise<Result<{ deleted: boolean }>> {
    try {
      const raw = await this.#native.delete(this.#name, key);
      return ok(JSON.parse(raw) as { deleted: boolean });
    } catch (e) {
      return err(e instanceof Error ? e : new Error(String(e)));
    }
  }

  /** List entries (optionally filtered by key prefix). */
  async list(prefix = ""): Promise<Result<ListEntry[]>> {
    try {
      const raw = await this.#native.list(this.#name, prefix);
      const parsed = JSON.parse(raw) as Array<{
        key: string;
        size: number;
        modifiedAt: number;
      }>;
      return ok(
        parsed.map((e) => ({
          key: e.key,
          size: e.size,
          modifiedAt: new Date(e.modifiedAt),
        })),
      );
    } catch (e) {
      return err(e instanceof Error ? e : new Error(String(e)));
    }
  }
}

/** Convenience factory — `bucket("uploads")` is the ergonomic entry point. */
export function bucket(name: string): Bucket {
  return new Bucket(name);
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

async function toBytes(
  body: Uint8Array | ArrayBuffer | string | Blob,
): Promise<Uint8Array> {
  if (body instanceof Uint8Array) return body;
  if (body instanceof ArrayBuffer) return new Uint8Array(body);
  if (typeof body === "string") return new TextEncoder().encode(body);
  // Blob
  const buf = await body.arrayBuffer();
  return new Uint8Array(buf);
}

function bytesToBase64(bytes: Uint8Array): string {
  // btoa is ASCII-only; chunk to avoid call-stack limits on large inputs.
  let binary = "";
  const chunk = 0x8000;
  for (let i = 0; i < bytes.length; i += chunk) {
    binary += String.fromCharCode(...bytes.subarray(i, i + chunk));
  }
  return btoa(binary);
}

function base64ToBytes(b64: string): Uint8Array {
  const binary = atob(b64);
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) out[i] = binary.charCodeAt(i);
  return out;
}
