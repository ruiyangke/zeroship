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
  // Streaming surface (proposal "env.storage streaming through V8"). These
  // back the streaming put/get below; the buffered forms above stay for
  // small objects.
  putStream(
    bucket: string,
    key: string,
    body: ReadableStream<Uint8Array>,
    contentType?: string,
  ): Promise<string>;
  getStream(bucket: string, key: string): Promise<string>;
  readChunk(streamId: number): Promise<Uint8Array | undefined>;
  cancelStream(streamId: number): Promise<void>;
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

export interface GetStreamResult {
  /**
   * The object's bytes as a `ReadableStream<Uint8Array>`. Memory stays
   * bounded by your read rate — the whole object is never buffered.
   */
  body: ReadableStream<Uint8Array>;
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

  /**
   * Store an object at `key`.
   *
   * Accepts in-memory bodies (`Uint8Array` / `ArrayBuffer` / `string`) which
   * go through the buffered native path, OR a `ReadableStream` / `Blob` which
   * streams chunk-by-chunk into the backend (S3 multipart / LocalFs
   * temp-file + atomic rename) with no whole-object buffering. Use a stream
   * for large uploads.
   */
  async put(
    key: string,
    body: Uint8Array | ArrayBuffer | string | Blob | ReadableStream<Uint8Array>,
    opts: { contentType?: string } = {},
  ): Promise<Result<PutResult>> {
    try {
      // Stream sources go straight to the streaming native path; nothing is
      // buffered whole-object.
      if (body instanceof ReadableStream) {
        return await this.#putStream(key, body, opts.contentType);
      }
      if (typeof Blob !== "undefined" && body instanceof Blob) {
        return await this.#putStream(key, body.stream(), opts.contentType ?? (body.type || undefined));
      }
      const bytes = await toBytes(body);
      const b64 = bytesToBase64(bytes);
      const raw = await this.#native.put(this.#name, key, b64, opts.contentType);
      const parsed = JSON.parse(raw) as PutResult;
      return ok(parsed);
    } catch (e) {
      return err(e instanceof Error ? e : new Error(String(e)));
    }
  }

  /**
   * Stream an object up from a `ReadableStream<Uint8Array>`. The buffered
   * `put` delegates here for stream / Blob bodies; call it directly when you
   * already hold a stream (e.g. a `fetch` response body).
   */
  async putStream(
    key: string,
    body: ReadableStream<Uint8Array>,
    opts: { contentType?: string } = {},
  ): Promise<Result<PutResult>> {
    return this.#putStream(key, body, opts.contentType);
  }

  async #putStream(
    key: string,
    body: ReadableStream<Uint8Array>,
    contentType?: string,
  ): Promise<Result<PutResult>> {
    try {
      const raw = await this.#native.putStream(this.#name, key, body, contentType);
      return ok(JSON.parse(raw) as PutResult);
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

  /**
   * Fetch an object as a stream. Returns `{data: null}` if the key doesn't
   * exist; otherwise `{ body, contentType, size }` where `body` is a
   * `ReadableStream<Uint8Array>` you pull at your own pace — the whole object
   * is never buffered in memory.
   */
  async getStream(key: string): Promise<Result<GetStreamResult | null>> {
    try {
      const raw = await this.#native.getStream(this.#name, key);
      if (raw === "null" || raw === null) return ok(null);
      const handle = JSON.parse(raw) as {
        streamId: number;
        contentType: string | null;
        size: number;
      };
      const native = this.#native;
      const streamId = handle.streamId;
      let cancelled = false;
      const body = new ReadableStream<Uint8Array>({
        async pull(controller) {
          const chunk = await native.readChunk(streamId);
          if (chunk === undefined || chunk === null) {
            controller.close();
            return;
          }
          controller.enqueue(chunk);
        },
        async cancel() {
          if (cancelled) return;
          cancelled = true;
          await native.cancelStream(streamId);
        },
      });
      return ok({ body, contentType: handle.contentType, size: handle.size });
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
