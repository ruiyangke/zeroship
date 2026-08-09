"use server";

// auth-uploads-kv - an authenticated file drop.
//
// This is the INTEGRATION example: `env.auth` (who is calling) + `env.storage`
// (the bytes) + `env.kv` (the rate limit) + RPC (the wire), in one app. Every
// other example in `examples/` exercises exactly one primitive, so none of
// them can show the seams that only exist when two primitives sit next to each
// other:
//
//   1. Identity decides the object key. The client never names a namespace,
//      so there is nothing for it to forge - and a read BY KEY has to re-derive
//      the same namespace or it is a cross-tenant disclosure.
//   2. KV gates the write. The counter is reserved BEFORE the bytes are
//      written, because a limit you check after the work is not a limit.
//   3. There is no transaction across KV and storage. If step 2 succeeds and
//      step 3 throws, the app is the only thing that can put the counter back.
//      This file does that explicitly; see `releaseUploadSlot`.
//
// It deliberately does NOT use `env.db`. Storage and KV need no schema, so
// this app runs under `pnpm dev` today; a migration-first `env.db` app does
// not (see examples/auth-notes-db and its README).
//
// Procedures (explicit dotted RPC ids -> /__zeroship/v1/<id>):
//
//   files.upload    mutation - rate-limit, then write bytes under MY prefix
//   files.list      query - the objects under MY prefix, newest first
//   files.download  query - one object BY KEY, still scoped to MY prefix
//   files.delete    mutation - remove one object BY KEY, same scoping
//   files.quota     query - the caller's current window usage

import { auth } from "@zeroship/auth";
import { kv, type Result as KvResult } from "@zeroship/kv";
import { bucket, type Result as StorageResult } from "@zeroship/storage";
import { mutation, query } from "@zeroship/rpc/server";
import { z } from "@zeroship/server";

const BUCKET = "drop";
const KV_PREFIX = "uploads:";

/** Uploads allowed per user per window. Small on purpose: it has to be
 *  reachable from a smoke test without uploading a thousand files. */
const UPLOAD_LIMIT = 5;
const RATE_WINDOW_MS = 60_000;

/**
 * App-side ceiling on a single upload - 512 KiB, and the number is measured,
 * not guessed.
 *
 * The runtime's HTTP server caps a REQUEST BODY at 1 MiB
 * (`MAX_BODY_BYTES`, crates/runtime/src/core/serve.rs), and base64 inflates
 * bytes by 4/3. So the real ceiling on a buffered upload over the JSON RPC
 * wire is ~768 KiB of payload, NOT the 16 MiB the buffered `env.storage.put`
 * would allow. 512 KiB keeps the whole request (~683 KB of base64 plus the
 * envelope) comfortably under that, which means this app's own 413 is
 * reachable - a limit the transport eats before the handler sees it is not a
 * limit the app can be said to enforce.
 *
 * Anything genuinely large should go through `putStream`, which never
 * materialises the object in a request body at all.
 */
const MAX_UPLOAD_BYTES = 512 * 1024;

const store = () => bucket(BUCKET);
const counters = () => kv.namespace(KV_PREFIX);

// ---------------------------------------------------------------------------
// Result unwrapping. Both SDKs return `{ data, error }` rather than throwing.
// ---------------------------------------------------------------------------

function mustKv<T>(r: KvResult<T>): T {
  if (r.error) throw r.error;
  return r.data as T;
}

function mustStorage<T>(r: StorageResult<T>): T {
  if (r.error) throw r.error;
  return r.data as T;
}

// ---------------------------------------------------------------------------
// Errors. The RPC error mapper reads `.status`, so a plain `Error` becomes a
// 500 - which is the wrong answer for every case below.
// ---------------------------------------------------------------------------

function httpError(status: number, code: string, message: string): Error {
  return Object.assign(new Error(message), { status, code });
}

function notFound(): never {
  // 404, not 403. "That object exists but is not yours" is itself a
  // disclosure - it confirms someone else's key. The check below cannot tell
  // the two cases apart anyway, which is exactly the property we want.
  throw httpError(404, "NOT_FOUND", "File not found");
}

// ---------------------------------------------------------------------------
// Identity -> namespace. The ONE place ownership is decided.
// ---------------------------------------------------------------------------

/**
 * The authenticated caller's per-app subject (`pws_...`), or a 401.
 *
 * `auth.requireUser()` exists but throws a plain `Error` with no `.status`,
 * which the RPC mapper turns into a 500. An anonymous caller deserves a 401,
 * so gate on `getUser()` and throw a status-bearing error.
 */
function callerId(): string {
  const user = auth.getUser();
  if (!user) throw httpError(401, "UNAUTHENTICATED", "Sign in to use the file drop");
  return user.id;
}

/** Every object this user owns lives under exactly this prefix. */
function prefixFor(userId: string): string {
  return `u/${userId}/`;
}

/**
 * Re-derive the caller's namespace and refuse any key outside it.
 *
 * This is the cross-tenant check. `files.list` filtering by prefix is NOT
 * enough: a by-key read that trusts the key it was handed serves any object
 * in the bucket to anyone who can name one, and user ids are not secrets.
 * The key arrives from the client, so it is checked against identity here and
 * nowhere else.
 */
function assertOwned(userId: string, key: string): void {
  const prefix = prefixFor(userId);
  // `..` can never appear in a key we minted; a key carrying one is a
  // traversal attempt, and the platform rejects it too (`storage: invalid
  // key`). Refuse it here so the answer is a 404 rather than a 500.
  if (!key.startsWith(prefix) || key.includes("..") || key.includes("//")) notFound();
  // A prefix match alone would let `u/<me>/` be passed as a key. Require a
  // non-empty object name after it.
  if (key.length <= prefix.length) notFound();
}

// ---------------------------------------------------------------------------
// Key encoding. Storage has no metadata table, so the display name rides in
// the key: `u/<userId>/<uploadId>__<safeName>`. `uploadId` is base36 with a
// single `-`, so the FIRST `__` is always the boundary.
// ---------------------------------------------------------------------------

function safeName(name: string): string {
  const cleaned = name
    .trim()
    .replace(/[^A-Za-z0-9._-]+/g, "-")
    .replace(/^[-.]+|-+$/g, "")
    .slice(0, 64);
  return cleaned || "upload.bin";
}

function newKey(userId: string, name: string): string {
  const id = `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
  return `${prefixFor(userId)}${id}__${safeName(name)}`;
}

function nameFromKey(key: string): string {
  const tail = key.slice(key.lastIndexOf("/") + 1);
  const sep = tail.indexOf("__");
  return sep === -1 ? tail : tail.slice(sep + 2);
}

// ---------------------------------------------------------------------------
// base64 <-> bytes. The RPC wire is JSON; bytes cross it base64-encoded.
// ---------------------------------------------------------------------------

function bytesToBase64(bytes: Uint8Array): string {
  let binary = "";
  const chunk = 0x8000;
  for (let i = 0; i < bytes.length; i += chunk) {
    binary += String.fromCharCode(...bytes.subarray(i, i + chunk));
  }
  return btoa(binary);
}

/** Throws on malformed input - `atob` is strict, and so are we. */
function base64ToBytes(b64: string): Uint8Array {
  let binary: string;
  try {
    binary = atob(b64);
  } catch {
    throw httpError(400, "INVALID_ARGUMENT", "contentBase64 is not valid base64");
  }
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) out[i] = binary.charCodeAt(i);
  return out;
}

// ---------------------------------------------------------------------------
// The KV rate limit - a fixed window, reserved before the write.
// ---------------------------------------------------------------------------

export type Quota = {
  used: number;
  limit: number;
  remaining: number;
  /** ms until the window resets, or null if no window is open. */
  resetMs: number | null;
};

function windowKey(userId: string, now = Date.now()): string {
  return `rate:${userId}:${Math.floor(now / RATE_WINDOW_MS)}`;
}

async function readQuota(userId: string): Promise<Quota> {
  const key = windowKey(userId);
  const used = mustKv(await counters().get<number>(key)) ?? 0;
  const ttl = mustKv(await counters().ttl(key));
  return {
    used,
    limit: UPLOAD_LIMIT,
    remaining: Math.max(0, UPLOAD_LIMIT - used),
    resetMs: ttl?.ttlMs ?? null,
  };
}

/**
 * Claim one upload slot, atomically.
 *
 * `incr` is a single native op, so two concurrent uploads cannot both read
 * `used = LIMIT - 1` and both proceed. `ttlMs` applies only when the call
 * CREATES the key, which is precisely the fixed-window shape: the first
 * upload of a window starts the clock and later ones inherit it.
 *
 * An over-limit attempt is NOT released. The attempt is the thing being
 * limited, so hammering the endpoint pushes the counter further past the
 * limit rather than resetting it.
 */
async function reserveUploadSlot(userId: string): Promise<void> {
  const key = windowKey(userId);
  const used = mustKv(await counters().incr(key, { by: 1, ttlMs: RATE_WINDOW_MS }));
  if (used > UPLOAD_LIMIT) {
    const ttl = mustKv(await counters().ttl(key));
    throw Object.assign(
      httpError(
        429,
        "RATE_LIMITED",
        `Upload limit reached (${UPLOAD_LIMIT} per ${RATE_WINDOW_MS / 1000}s)`,
      ),
      { retryAfterMs: ttl?.ttlMs ?? RATE_WINDOW_MS },
    );
  }
}

/**
 * Give the slot back when the upload that claimed it did not happen.
 *
 * There is NO transaction spanning `env.kv` and `env.storage`. If the reserve
 * commits and the write then throws, this compensating decrement is the only
 * thing that stops a failed upload from consuming quota - nothing in the
 * platform will do it for us.
 *
 * The `has` guard matters: `incr(key, { by: -1 })` on an ABSENT key creates it
 * at -1 with a fresh TTL, so a release arriving after its window expired would
 * hand the next window a free extra upload. The guard is not atomic with the
 * decrement (the window can roll between the two calls), and the failure is
 * one-sided in the safe direction: worst case a release is dropped and the
 * caller loses a slot they never used. Fixing that properly needs a
 * compare-and-decrement the KV surface does not currently expose.
 */
async function releaseUploadSlot(userId: string): Promise<void> {
  const key = windowKey(userId);
  const present = mustKv(await counters().has(key));
  if (!present) return;
  mustKv(await counters().incr(key, { by: -1 }));
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

export type FileInfo = {
  /** The full storage key. It is the handle for download/delete, and it is
   *  namespaced by owner - which is why every by-key call re-checks it. */
  key: string;
  name: string;
  size: number;
  uploadedAt: number;
};

export type UploadResult = {
  file: FileInfo;
  quota: Quota;
};

export type ListResult = {
  owner: string;
  files: FileInfo[];
};

export type DownloadResult = {
  key: string;
  name: string;
  size: number;
  contentType: string | null;
  contentBase64: string;
};

export type DeleteResult = {
  key: string;
  deleted: boolean;
};

// ---------------------------------------------------------------------------
// files.upload
// ---------------------------------------------------------------------------

export const upload = mutation(
  async ({
    name,
    contentBase64,
    contentType,
  }: {
    name: string;
    contentBase64: string;
    contentType?: string;
  }): Promise<UploadResult> => {
    const userId = callerId();

    // Cheap, caller-fault rejections happen BEFORE the reserve, so a request
    // that was never going to work does not burn a slot. (base64 inflates by
    // 4/3, so this bound is conservative by design.)
    if (contentBase64.length > MAX_UPLOAD_BYTES * 2) {
      throw httpError(413, "PAYLOAD_TOO_LARGE", `Uploads are capped at ${MAX_UPLOAD_BYTES} bytes`);
    }

    // Reserve first. A limit checked after the write is not a limit.
    await reserveUploadSlot(userId);

    try {
      const bytes = base64ToBytes(contentBase64);
      if (bytes.length === 0) {
        throw httpError(400, "INVALID_ARGUMENT", "Refusing to store an empty file");
      }
      if (bytes.length > MAX_UPLOAD_BYTES) {
        throw httpError(413, "PAYLOAD_TOO_LARGE", `Uploads are capped at ${MAX_UPLOAD_BYTES} bytes`);
      }

      const key = newKey(userId, name);
      const put = mustStorage(await store().put(key, bytes, { contentType }));
      return {
        file: { key: put.key, name: nameFromKey(put.key), size: put.size, uploadedAt: Date.now() },
        quota: await readQuota(userId),
      };
    } catch (e) {
      // The compensating half of the reserve. Everything between the reserve
      // and the successful `put` lands here: a malformed body, an oversize
      // body, a storage backend that refused the write.
      await releaseUploadSlot(userId);
      throw e;
    }
  },
  {
    id: "files.upload",
    input: z.object({
      name: z.string().min(1).max(200),
      contentBase64: z.string(),
      contentType: z.string().max(120).optional(),
    }),
  },
);

// ---------------------------------------------------------------------------
// files.list
// ---------------------------------------------------------------------------

export const list = query(
  async (): Promise<ListResult> => {
    const userId = callerId();
    const prefix = prefixFor(userId);

    // The prefix IS the query. Someone else's object is never fetched, not
    // fetched-and-filtered.
    const files: FileInfo[] = [];
    for await (const entry of store().listAll(prefix, { limit: 100 })) {
      files.push({
        key: entry.key,
        name: nameFromKey(entry.key),
        size: entry.size,
        uploadedAt: entry.modifiedAt.getTime(),
      });
      if (files.length >= 200) break;
    }
    files.sort((a, b) => b.uploadedAt - a.uploadedAt);
    return { owner: userId, files };
  },
  { id: "files.list" },
);

// ---------------------------------------------------------------------------
// files.download - the one that matters
// ---------------------------------------------------------------------------

export const download = query(
  async ({ key }: { key: string }): Promise<DownloadResult> => {
    const userId = callerId();
    // BEFORE the read, not after. Reading first and then deciding whether to
    // return it is one refactor away from a disclosure, and it has already
    // done the I/O by the time it decides.
    assertOwned(userId, key);

    const obj = mustStorage(await store().get(key));
    if (!obj) notFound();
    return {
      key,
      name: nameFromKey(key),
      size: obj.size,
      contentType: obj.contentType,
      contentBase64: bytesToBase64(obj.bytes),
    };
  },
  {
    id: "files.download",
    input: z.object({ key: z.string().min(1).max(400) }),
  },
);

// ---------------------------------------------------------------------------
// files.delete
// ---------------------------------------------------------------------------

export const remove = mutation(
  async ({ key }: { key: string }): Promise<DeleteResult> => {
    const userId = callerId();
    assertOwned(userId, key);
    const res = mustStorage(await store().delete(key));
    // Deleting does NOT refund a slot: the counter limits upload ATTEMPTS per
    // window, not stored files. Refunding on delete would make the limit
    // trivially bypassable by upload/delete/upload.
    return { key, deleted: res.deleted };
  },
  {
    id: "files.delete",
    input: z.object({ key: z.string().min(1).max(400) }),
  },
);

// ---------------------------------------------------------------------------
// files.quota
// ---------------------------------------------------------------------------

export const quota = query(
  async (): Promise<Quota & { owner: string }> => {
    const userId = callerId();
    return { owner: userId, ...(await readQuota(userId)) };
  },
  { id: "files.quota" },
);
