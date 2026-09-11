"use server";

// storage-gallery — real-world example exercising @zeroship/storage
// (the `env.storage` native primitive) end to end.
//
// What this file demonstrates:
//   • bucket(name)          — the ergonomic Bucket entry point
//   • bucket.put(key, body) — store bytes / text (+ optional contentType)
//   • bucket.get(key)       — read an object back (bytes + contentType + size)
//   • bucket.getText(key)   — convenience UTF-8 read
//   • bucket.list(prefix, {cursor, limit}) — one PAGE of keys (+ size +
//                             modifiedAt) plus a cursor when more remain
//   • bucket.delete(key)    — remove an object
//
// Wire IDs are explicit and dotted: each export becomes an RPC procedure
// reachable at /__zeroship/v1/<id>. The RPC wire is JSON, so binary payloads
// cross the boundary as base64 strings; this module does the base64 plumbing
// so the procedures stay JSON-clean.

import { bucket, type Result } from "@zeroship/storage";
import { mutation, query } from "@zeroship/rpc/server";
import { Checksum } from "./checksum";

const BUCKET = "gallery";

const store = () => bucket(BUCKET);

function must<T>(r: Result<T>): T {
  if (r.error) throw r.error;
  return r.data as T;
}

function bytesToBase64(bytes: Uint8Array): string {
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

export type PutResult = {
  bucket: string;
  key: string;
  size: number;
};

export type GetResult = {
  key: string;
  found: boolean;
  /** Decoded UTF-8 text, when the stored bytes are valid UTF-8. */
  text: string | null;
  /** Raw bytes, base64-encoded for the JSON wire. */
  bytesBase64: string | null;
  contentType: string | null;
  size: number | null;
};

export type ListEntryDto = {
  key: string;
  size: number;
  modifiedAt: number;
};

export type ListPageDto = {
  entries: ListEntryDto[];
  /** Non-null iff more keys remain — pass it back as `cursor` for the next page. */
  cursor: string | null;
};

export type DeleteResult = {
  key: string;
  deleted: boolean;
};

// ---------------------------------------------------------------------------
// put — store text or arbitrary bytes (base64) under `key`.
// Provide EITHER `text` (UTF-8) or `bytesBase64`; `text` wins if both given.
// ---------------------------------------------------------------------------
export const put = mutation(
  async ({
    key,
    text,
    bytesBase64,
    contentType,
  }: {
    key: string;
    text?: string;
    bytesBase64?: string;
    contentType?: string;
  }): Promise<PutResult> => {
    if (!key) {
      throw Object.assign(new Error("key is required"), { code: "INVALID_ARGUMENT" });
    }
    const body: Uint8Array =
      text != null
        ? new TextEncoder().encode(text)
        : bytesBase64 != null
          ? base64ToBytes(bytesBase64)
          : new Uint8Array(0);
    const res = must(await store().put(key, body, { contentType }));
    return { bucket: res.bucket, key: res.key, size: res.size };
  },
  { id: "gallery.put" },
);

// ---------------------------------------------------------------------------
// get — read an object back. Returns { found: false } when the key is absent.
// ---------------------------------------------------------------------------
export const get = query(
  async ({ key }: { key: string }): Promise<GetResult> => {
    const obj = must(await store().get(key));
    if (!obj) {
      return { key, found: false, text: null, bytesBase64: null, contentType: null, size: null };
    }
    let text: string | null = null;
    try {
      text = new TextDecoder("utf-8", { fatal: true }).decode(obj.bytes);
    } catch {
      text = null; // not valid UTF-8 — caller can use bytesBase64
    }
    return {
      key,
      found: true,
      text,
      bytesBase64: bytesToBase64(obj.bytes),
      contentType: obj.contentType,
      size: obj.size,
    };
  },
  { id: "gallery.get" },
);

// ---------------------------------------------------------------------------
// list — one page of keys (optionally filtered by prefix).
//
// `list` is paginated on purpose: an "enumerate everything" call would make
// one request cost whatever the bucket happens to hold. The returned `cursor`
// is the truncation signal — non-null means more keys remain, so a client that
// wants them all loops until it comes back null. (`store().listAll(prefix)` is
// the SDK's lazy async-iterator over exactly that loop.)
// ---------------------------------------------------------------------------
export const list = query(
  async ({
    prefix = "",
    cursor,
    limit,
  }: {
    prefix?: string;
    cursor?: string;
    limit?: number;
  }): Promise<ListPageDto> => {
    const page = must(await store().list(prefix, { cursor, limit }));
    return {
      entries: page.entries.map((e) => ({
        key: e.key,
        size: e.size,
        modifiedAt: e.modifiedAt.getTime(),
      })),
      cursor: page.cursor,
    };
  },
  { id: "gallery.list" },
);

// ---------------------------------------------------------------------------
// delete — remove an object. `{ deleted: false }` if it wasn't there.
// ---------------------------------------------------------------------------
export const remove = mutation(
  async ({ key }: { key: string }): Promise<DeleteResult> => {
    const res = must(await store().delete(key));
    return { key, deleted: res.deleted };
  },
  { id: "gallery.delete" },
);

// ---------------------------------------------------------------------------
// Streaming round-trip (S3 multipart on the prod backend).
//
// The whole point of the streaming path is that the object never crosses the
// JSON RPC wire whole — so these procedures GENERATE the large body on the
// server from a tiny `{ sizeBytes, seed }` request, stream it up with
// `putStream`, and report only its size + checksum. The download counterpart
// streams it back with `getStream`, drains it incrementally, and reports size
// + checksum. The tests compare streaming checksums and physical object length.
// Pattern copies and incremental checksums keep memory bounded. Cooperative
// pauses respect the runtime's sustained CPU budget while scanning large objects.
// ---------------------------------------------------------------------------

// One period of the deterministic pattern (a prime length so chunk/part
// boundaries land at varying phases — a misordered or short part shifts the
// checksum). Built once, cheaply.
const PATTERN_PERIOD = 4099;
function buildPattern(seed: number): Uint8Array {
  const p = new Uint8Array(PATTERN_PERIOD);
  for (let i = 0; i < PATTERN_PERIOD; i++) p[i] = (seed * 31 + i) & 0xff;
  return p;
}

// Fill `out` with the repeating pattern starting at absolute offset `start`,
// using native `set` block copies (no per-byte loop over `out`).
function fillFromPattern(out: Uint8Array, pattern: Uint8Array, start: number): void {
  let written = 0;
  while (written < out.length) {
    const phase = (start + written) % PATTERN_PERIOD;
    const slice = pattern.subarray(phase, Math.min(PATTERN_PERIOD, phase + (out.length - written)));
    out.set(slice, written);
    written += slice.length;
  }
}

class StreamPacer {
  private started = performance.now();
  private processed = 0;

  async checkpoint(bytes: number): Promise<void> {
    this.processed += bytes;
    if (this.processed < 1024 * 1024) return;
    const elapsed = performance.now() - this.started;
    await new Promise<void>((resolve) => setTimeout(resolve, Math.max(1, elapsed)));
    this.started = performance.now();
    this.processed = 0;
  }
}

export type PutLargeResult = {
  key: string;
  size: number;
  checksum: string;
};

// putLarge — stream a `sizeBytes` object up via a chunked ReadableStream (the
// whole object is never buffered in JS), computing the checksum incrementally
// as each chunk is produced.
export const putLarge = mutation(
  async ({
    key,
    sizeBytes,
    seed = 1,
    chunkBytes = 1024 * 1024,
  }: {
    key: string;
    sizeBytes: number;
    seed?: number;
    chunkBytes?: number;
  }): Promise<PutLargeResult> => {
    if (!key) {
      throw Object.assign(new Error("key is required"), { code: "INVALID_ARGUMENT" });
    }
    if (!Number.isInteger(sizeBytes) || sizeBytes <= 0) {
      throw Object.assign(new Error("sizeBytes must be a positive integer"), {
        code: "INVALID_ARGUMENT",
      });
    }

    const pattern = buildPattern(seed);
    const checksum = new Checksum();
    const pacer = new StreamPacer();
    let offset = 0;
    const body = new ReadableStream<Uint8Array>({
      async pull(controller) {
        if (offset >= sizeBytes) {
          controller.close();
          return;
        }
        const len = Math.min(chunkBytes, sizeBytes - offset);
        const chunk = new Uint8Array(len);
        fillFromPattern(chunk, pattern, offset);
        checksum.update(chunk);
        offset += len;
        controller.enqueue(chunk);
        await pacer.checkpoint(chunk.length);
      },
    });

    const res = must(
      await store().putStream(key, body, { contentType: "application/octet-stream" }),
    );
    return { key: res.key, size: res.size, checksum: checksum.hex() };
  },
  { id: "gallery.putLarge" },
);

export type GetLargeResult = {
  key: string;
  found: boolean;
  size: number | null;
  checksum: string | null;
};

// getLargeHash — open the object as a stream, drain it incrementally (bounded
// by read rate; chunks are checksummed and dropped, never accumulated), and
// report its size + checksum without ever sending the bytes over the wire.
export const getLargeHash = query(
  async ({ key }: { key: string }): Promise<GetLargeResult> => {
    const res = must(await store().getStream(key));
    if (!res) return { key, found: false, size: null, checksum: null };

    const reader = res.body.getReader();
    const checksum = new Checksum();
    const pacer = new StreamPacer();
    let total = 0;
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      if (value && value.length) {
        checksum.update(value);
        total += value.length;
        await pacer.checkpoint(value.length);
      }
    }
    return { key, found: true, size: total, checksum: checksum.hex() };
  },
  { id: "gallery.getLargeHash" },
);
