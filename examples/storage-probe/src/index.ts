"use server";

// storage-probe -- the `env.storage` fixture for the dev-vs-deployed walk.
//
// Why a new app rather than reusing an existing one: the only storage example
// that could be driven on both sides was `examples/auth-uploads-kv`, and its
// smoke logs in through the dev-auth provider, which is dev-only by
// construction. Storage itself has NO auth dependency, so scenario 5 was
// blocked by a coupling in the fixture, not by the platform. This app removes
// the coupling: no login, no per-user data, every procedure anonymous.
//
// Shape rules, both learned the expensive way (docs/pilot/e2e-scenarios.md):
//
//   1. The file starts with "use server". Without it the build SUCCEEDS,
//      reports `0 server functions`, still emits a manifest declaring every
//      RPC resource, and every procedure 404s at runtime (#167).
//   2. `src/server/config.ts` opts every procedure into anonymous access.
//      Without it each one resolves to `auth: "user"`, which is green under
//      `pnpm dev` and 401 for every call once deployed (#163).
//
// Every procedure returns plain JSON that a shell script can diff verbatim
// against the same procedure run on the other side. That constrains the
// design: no timestamps, no cursors, no backend-assigned identifiers in the
// output, because those are volatile or opaque and would make the two sides
// differ for reasons that are not defects. What IS returned is the part the
// contract talks about -- sizes, bytes, content types, key order, page
// boundaries, truncation signals.

import { bucket, type ListPage, type Result } from "@zeroship/storage";
import { mutation, query } from "@zeroship/rpc/server";

const BUCKET = "probe";
/** Every key this app touches lives under here, so `reset` can be exact. */
const ROOT = "sp/";

const store = () => bucket(BUCKET);

function must<T>(r: Result<T>): T {
  if (r.error) throw r.error;
  return r.data as T;
}

// ---------------------------------------------------------------------------
// Deterministic bodies + an order-sensitive checksum.
//
// The checksum is position-weighted so a dropped, duplicated, reordered or
// short chunk changes the result. Comparing only lengths would pass on a
// backend that returned the right number of wrong bytes; comparing the decoded
// text only works for text. This works for both and is cheap enough to run
// over a multi-megabyte stream inside the app CPU limit.
// ---------------------------------------------------------------------------

const PATTERN_PERIOD = 4099; // prime, so chunk boundaries land at varying phase

function buildPattern(seed: number): Uint8Array {
  const p = new Uint8Array(PATTERN_PERIOD);
  for (let i = 0; i < PATTERN_PERIOD; i++) p[i] = (seed * 31 + i) & 0xff;
  return p;
}

function fillFromPattern(out: Uint8Array, pattern: Uint8Array, start: number): void {
  let written = 0;
  while (written < out.length) {
    const phase = (start + written) % PATTERN_PERIOD;
    const slice = pattern.subarray(
      phase,
      Math.min(PATTERN_PERIOD, phase + (out.length - written)),
    );
    out.set(slice, written);
    written += slice.length;
  }
}

function updateChecksum(
  acc: { a: number; b: number },
  chunk: Uint8Array,
  absStart: number,
): void {
  let { a, b } = acc;
  for (let i = 0; i < chunk.length; i++) {
    a = (a + chunk[i]! * (((absStart + i) % 65521) + 1)) % 0xfffffffb;
    b = (b + a) % 0xfffffffb;
  }
  acc.a = a;
  acc.b = b;
}

function checksumHex(acc: { a: number; b: number }): string {
  return (
    (acc.a >>> 0).toString(16).padStart(8, "0") +
    (acc.b >>> 0).toString(16).padStart(8, "0")
  );
}

function checksumOf(bytes: Uint8Array): string {
  const acc = { a: 1, b: 0 };
  updateChecksum(acc, bytes, 0);
  return checksumHex(acc);
}

// ---------------------------------------------------------------------------
// probe.ping -- reachability, before anything reasons about storage.
//
// Without it a stack that never came up and a storage backend that is missing
// look alike: every procedure errors. This one touches no object.
// ---------------------------------------------------------------------------
export const ping = query(async () => ({ ok: true, bucket: BUCKET, root: ROOT }), {
  id: "probe.ping",
});

// ---------------------------------------------------------------------------
// probe.reset -- delete everything under ROOT, then prove it is gone.
//
// Returns `remaining`, not `deleted`: the number deleted depends on what the
// previous run left behind (dev's LocalFs dir persists across runs, a fresh
// deployed backend does not), and comparing that would report a different
// STARTING STATE as a divergence. `remaining` is 0 on any correct backend.
// ---------------------------------------------------------------------------
export const reset = mutation(
  async () => {
    const keys: string[] = [];
    for await (const e of store().listAll(ROOT, { limit: 100 })) keys.push(e.key);
    for (const k of keys) must(await store().delete(k));
    const after = must(await store().list(ROOT, { limit: 100 }));
    return { remaining: after.entries.length, moreAfter: after.cursor !== null };
  },
  { id: "probe.reset" },
);

// ---------------------------------------------------------------------------
// probe.text -- put + get round trip for text, with a content type.
// ---------------------------------------------------------------------------
const TEXT = "hello storage probe -- éà中文 -- line1\nline2\n";

export const text = mutation(
  async () => {
    const key = `${ROOT}text/hello.txt`;
    const put = must(await store().put(key, TEXT, { contentType: "text/plain; charset=utf-8" }));
    const got = must(await store().get(key));
    const asText = must(await store().getText(key));
    return {
      putSize: put.size,
      putKey: put.key,
      putBucket: put.bucket,
      found: got !== null,
      getSize: got?.size ?? null,
      contentType: got?.contentType ?? null,
      textMatches: asText === TEXT,
      checksum: got ? checksumOf(got.bytes) : null,
    };
  },
  { id: "probe.text" },
);

// ---------------------------------------------------------------------------
// probe.binary -- the same round trip for bytes that are NOT valid UTF-8.
//
// All 256 byte values, so any backend that round-trips through a string
// somewhere corrupts the high half and the checksum says so.
// ---------------------------------------------------------------------------
export const binary = mutation(
  async () => {
    const key = `${ROOT}bin/all-bytes.dat`;
    const body = new Uint8Array(256);
    for (let i = 0; i < 256; i++) body[i] = i;
    const expected = checksumOf(body);
    const put = must(await store().put(key, body, { contentType: "application/octet-stream" }));
    const got = must(await store().get(key));
    return {
      putSize: put.size,
      found: got !== null,
      getSize: got?.size ?? null,
      contentType: got?.contentType ?? null,
      checksum: got ? checksumOf(got.bytes) : null,
      bytesIdentical: got ? checksumOf(got.bytes) === expected : false,
    };
  },
  { id: "probe.binary" },
);

// ---------------------------------------------------------------------------
// probe.overwrite -- writing the same key twice replaces, and does not append,
// leave the old length, or keep the old content type.
//
// The second body is SHORTER than the first, which is the case that catches a
// backend that truncates lazily or reports the old size.
// ---------------------------------------------------------------------------
export const overwrite = mutation(
  async () => {
    const key = `${ROOT}overwrite/doc.txt`;
    const first = "first version, deliberately the longer one";
    const second = "second";
    must(await store().put(key, first, { contentType: "text/plain" }));
    const afterFirst = must(await store().get(key));
    must(await store().put(key, second, { contentType: "application/json" }));
    const afterSecond = must(await store().get(key));
    const list = must(await store().list(`${ROOT}overwrite/`, { limit: 10 }));
    return {
      firstSize: afterFirst?.size ?? null,
      firstType: afterFirst?.contentType ?? null,
      secondSize: afterSecond?.size ?? null,
      secondType: afterSecond?.contentType ?? null,
      secondText: afterSecond ? new TextDecoder().decode(afterSecond.bytes) : null,
      // An overwrite must not create a second entry.
      entriesUnderPrefix: list.entries.length,
    };
  },
  { id: "probe.overwrite" },
);

// ---------------------------------------------------------------------------
// probe.deleteAbsent -- delete, then read; and delete again.
//
// Two separate contract questions:
//   - what a GET of an absent key returns (the SDK says `{data: null}`, so the
//     app sees `found: false`, not a throw);
//   - what a DELETE of an absent key reports. The SDK types it
//     `{deleted: boolean}` and documents "`{deleted: false}` if it wasn't
//     there", which is only answerable per backend: S3's DeleteObject succeeds
//     unconditionally and does not say whether anything was there.
// ---------------------------------------------------------------------------
export const deleteAbsent = mutation(
  async () => {
    const key = `${ROOT}delete/gone.txt`;
    must(await store().put(key, "about to vanish"));
    const firstDelete = must(await store().delete(key));
    const afterDelete = must(await store().get(key));
    const secondDelete = must(await store().delete(key));
    const neverExisted = must(await store().delete(`${ROOT}delete/never-written.txt`));
    const listed = must(await store().list(`${ROOT}delete/`, { limit: 10 }));
    return {
      firstDeleted: firstDelete.deleted,
      getAfterDeleteFound: afterDelete !== null,
      secondDeleted: secondDelete.deleted,
      neverExistedDeleted: neverExisted.deleted,
      entriesUnderPrefix: listed.entries.length,
    };
  },
  { id: "probe.deleteAbsent" },
);

// ---------------------------------------------------------------------------
// probe.contentTypes -- content type round trip across several values, plus
// the "not supplied" case.
//
// This is the seam with a KNOWN history: LocalFs once dropped contentType
// while S3 kept it, and nothing compared the two. The `given: null` row is the
// interesting one -- the SDK types contentType as `string | null` and says
// "may be null", so a backend that substitutes a default is not obviously
// wrong, it is just different, and only a two-sided diff shows it.
// ---------------------------------------------------------------------------
const CONTENT_TYPES: Array<string | null> = [
  "text/plain; charset=utf-8",
  "application/json",
  "image/png",
  null,
];

export const contentTypes = mutation(
  async () => {
    const rows: Array<{ given: string | null; got: string | null; size: number | null }> = [];
    for (let i = 0; i < CONTENT_TYPES.length; i++) {
      const given = CONTENT_TYPES[i]!;
      const key = `${ROOT}ctype/${i}.bin`;
      const body = `content-type probe ${i}`;
      must(
        given === null
          ? await store().put(key, body)
          : await store().put(key, body, { contentType: given }),
      );
      const got = must(await store().get(key));
      rows.push({ given, got: got?.contentType ?? null, size: got?.size ?? null });
    }
    return { rows };
  },
  { id: "probe.contentTypes" },
);

// ---------------------------------------------------------------------------
// probe.listPrefix -- a prefix filters, and the prefix is a literal, not a
// glob. Key ORDER is compared verbatim, because the SDK documents
// `list(prefix)` as returning entries "in ascending key order" -- unlike
// `@zeroship/kv`'s list, whose order is unspecified and therefore had to be
// sorted before comparison in the KV harness.
// ---------------------------------------------------------------------------
export const listPrefix = mutation(
  async () => {
    const base = `${ROOT}list/`;
    const writes: Array<[string, string]> = [
      [`${base}a/3.txt`, "aaa"],
      [`${base}a/1.txt`, "a"],
      [`${base}a/2.txt`, "aa"],
      [`${base}b/1.txt`, "bbbb"],
      [`${base}ab.txt`, "ab-not-in-a-slash"],
    ];
    for (const [k, v] of writes) must(await store().put(k, v));
    const underA = must(await store().list(`${base}a/`, { limit: 50 }));
    const underBase = must(await store().list(base, { limit: 50 }));
    const noPrefix = must(await store().list(ROOT, { limit: 200 }));
    return {
      aKeys: underA.entries.map((e) => e.key),
      aSizes: underA.entries.map((e) => e.size),
      aCursorNull: underA.cursor === null,
      baseKeys: underBase.entries.map((e) => e.key),
      // The whole-root listing must be a superset; only its COUNT is compared,
      // because it also contains whatever earlier procedures wrote and that is
      // sequence-dependent rather than backend-dependent.
      rootCount: noPrefix.entries.length,
    };
  },
  { id: "probe.listPrefix" },
);

// ---------------------------------------------------------------------------
// probe.listPaginate -- pagination is the truncation contract: `cursor` is
// non-null IFF more keys remain, and the pages concatenate to the whole set
// with nothing dropped and nothing repeated.
//
// The cursor VALUE is never returned: it is documented as opaque, and LocalFs
// and S3 mint different ones. What is compared is where the page boundaries
// fall and whether the last page correctly reports the end.
// ---------------------------------------------------------------------------
export const listPaginate = mutation(
  async () => {
    const base = `${ROOT}page/`;
    for (let i = 0; i < 5; i++) must(await store().put(`${base}k${i}.txt`, `v${i}`));
    const pages: string[][] = [];
    const cursorNonNull: boolean[] = [];
    let cursor: string | null = null;
    let guard = 0;
    do {
      // Explicitly typed: `cursor` is assigned from `page.cursor` inside the
      // loop that produces `page`, so inference would be circular.
      const page: ListPage = must(
        await store().list(base, { limit: 2, cursor: cursor ?? undefined }),
      );
      pages.push(page.entries.map((e) => e.key));
      cursorNonNull.push(page.cursor !== null);
      cursor = page.cursor;
      guard++;
    } while (cursor !== null && guard < 10);
    const flat = pages.flat();
    // listAll drives the same loop inside the SDK; it must see the same keys.
    const viaListAll: string[] = [];
    for await (const e of store().listAll(base, { limit: 2 })) viaListAll.push(e.key);
    return {
      pages,
      cursorNonNull,
      total: flat.length,
      unique: new Set(flat).size,
      listAllMatches: viaListAll.join(",") === flat.join(","),
    };
  },
  { id: "probe.listPaginate" },
);

// ---------------------------------------------------------------------------
// probe.listOvershoot -- a limit larger than the number of keys returns one
// page and a null cursor, rather than a cursor that then yields nothing.
// ---------------------------------------------------------------------------
export const listOvershoot = query(
  async () => {
    const page = must(await store().list(`${ROOT}page/`, { limit: 1000 }));
    return { count: page.entries.length, cursorNull: page.cursor === null };
  },
  { id: "probe.listOvershoot" },
);

// ---------------------------------------------------------------------------
// Streaming put and streaming get.
//
// The body is generated ON THE SERVER from a size, and only its checksum
// crosses the RPC wire -- the point of the streaming path is that the object
// never has to exist whole anywhere, so shipping it through JSON would defeat
// what is being tested.
//
// STREAM_BYTES is a megabyte in 64 KiB chunks: 16 chunks up, and a read side
// that must loop. Ten bytes would exercise the same code with a single chunk
// and could not tell a chunked reader from a whole-object one.
// ---------------------------------------------------------------------------
const STREAM_BYTES = 1024 * 1024;
const STREAM_CHUNK = 64 * 1024;
const STREAM_KEY = `${ROOT}stream/blob.bin`;

export const streamPut = mutation(
  async () => {
    const pattern = buildPattern(7);
    const acc = { a: 1, b: 0 };
    let offset = 0;
    let chunksSent = 0;
    const body = new ReadableStream<Uint8Array>({
      pull(controller) {
        if (offset >= STREAM_BYTES) {
          controller.close();
          return;
        }
        const len = Math.min(STREAM_CHUNK, STREAM_BYTES - offset);
        const chunk = new Uint8Array(len);
        fillFromPattern(chunk, pattern, offset);
        updateChecksum(acc, chunk, offset);
        offset += len;
        chunksSent++;
        controller.enqueue(chunk);
      },
    });
    const res = must(
      await store().putStream(STREAM_KEY, body, { contentType: "application/octet-stream" }),
    );
    return {
      key: res.key,
      size: res.size,
      expectedSize: STREAM_BYTES,
      chunksSent,
      checksum: checksumHex(acc),
    };
  },
  { id: "probe.streamPut" },
);

export const streamGet = query(
  async () => {
    const res = must(await store().getStream(STREAM_KEY));
    if (!res) return { found: false, size: null, checksum: null, contentType: null, multiChunk: null };
    const reader = res.body.getReader();
    const acc = { a: 1, b: 0 };
    let total = 0;
    let chunks = 0;
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      if (value && value.length) {
        updateChecksum(acc, value, total);
        total += value.length;
        chunks++;
      }
    }
    return {
      found: true,
      size: total,
      declaredSize: res.size,
      checksum: checksumHex(acc),
      contentType: res.contentType,
      // The chunk COUNT is backend-specific (read buffer size is not part of
      // any contract), so what is asserted is only that the read was chunked
      // at all rather than one whole-object delivery.
      multiChunk: chunks > 1,
    };
  },
  { id: "probe.streamGet" },
);

// ---------------------------------------------------------------------------
// probe.streamGetAbsent -- the streaming read of a key that is not there must
// report absence the same way the buffered read does, not throw and not hand
// back an empty stream that looks like an empty object.
// ---------------------------------------------------------------------------
export const streamGetAbsent = query(
  async () => {
    const res = must(await store().getStream(`${ROOT}stream/not-here.bin`));
    return { found: res !== null };
  },
  { id: "probe.streamGetAbsent" },
);

// ---------------------------------------------------------------------------
// probe.streamThenBuffered -- an object written by the STREAMING path must be
// readable by the BUFFERED path and report the same size and content type.
// A backend that stores streamed uploads differently (a multipart object, a
// temp file left un-renamed) diverges exactly here.
// ---------------------------------------------------------------------------
export const streamThenBuffered = query(
  async () => {
    const got = must(await store().get(STREAM_KEY));
    const listed = must(await store().list(`${ROOT}stream/`, { limit: 10 }));
    return {
      found: got !== null,
      size: got?.size ?? null,
      contentType: got?.contentType ?? null,
      checksum: got ? checksumOf(got.bytes) : null,
      listedKeys: listed.entries.map((e) => e.key),
      listedSizes: listed.entries.map((e) => e.size),
    };
  },
  { id: "probe.streamThenBuffered" },
);
