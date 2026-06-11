"use server";

// storage-gallery — real-world example exercising @zeroship/storage
// (the `env.storage` native primitive) end to end.
//
// What this file demonstrates:
//   • bucket(name)          — the ergonomic Bucket entry point
//   • bucket.put(key, body) — store bytes / text (+ optional contentType)
//   • bucket.get(key)       — read an object back (bytes + contentType + size)
//   • bucket.getText(key)   — convenience UTF-8 read
//   • bucket.list(prefix)   — enumerate keys (+ size + modifiedAt)
//   • bucket.delete(key)    — remove an object
//
// Wire IDs are explicit and dotted: each export becomes an RPC procedure
// reachable at /__zeroship/v1/<id>. The RPC wire is JSON, so binary payloads
// cross the boundary as base64 strings; this module does the base64 plumbing
// so the procedures stay JSON-clean.

import { bucket, type Result } from "@zeroship/storage";
import { mutation, query } from "@zeroship/rpc/server";

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
// list — enumerate keys (optionally filtered by prefix).
// ---------------------------------------------------------------------------
export const list = query(
  async ({ prefix = "" }: { prefix?: string }): Promise<ListEntryDto[]> => {
    const entries = must(await store().list(prefix));
    return entries.map((e) => ({
      key: e.key,
      size: e.size,
      modifiedAt: e.modifiedAt.getTime(),
    }));
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
