// @zeroship/storage — streaming put/get SDK contract.
//
// Exercises the SDK's streaming surface against a mock NativeStorage (the
// `nativeOverride` ctor seam): a ReadableStream/Blob `put` routes to
// `putStream`, and `getStream` reassembles via the `readChunk` pull loop and
// cancels via `cancelStream`. The real native↔backend path is covered by the
// Rust `e2e_streaming.rs` test; this locks the JS-side wiring.

import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { Bucket } from "../src/index.ts";

interface NativeStorage {
  put(bucket: string, key: string, bytesBase64: string, contentType?: string): Promise<string>;
  get(bucket: string, key: string): Promise<string>;
  delete(bucket: string, key: string): Promise<string>;
  list(
    bucket: string,
    prefix?: string,
    opts?: { cursor?: string; limit?: number },
  ): Promise<string>;
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

function streamFrom(chunks: Uint8Array[]): ReadableStream<Uint8Array> {
  return new ReadableStream<Uint8Array>({
    start(controller) {
      for (const c of chunks) controller.enqueue(c);
      controller.close();
    },
  });
}

async function drainStream(s: ReadableStream<Uint8Array>): Promise<Uint8Array> {
  const reader = s.getReader();
  const parts: Uint8Array[] = [];
  let total = 0;
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    parts.push(value);
    total += value.length;
  }
  const out = new Uint8Array(total);
  let off = 0;
  for (const p of parts) {
    out.set(p, off);
    off += p.length;
  }
  return out;
}

/** A mock native backed by an in-memory key→bytes map with a streaming surface. */
function mockNative(): { native: NativeStorage; cancels: number[] } {
  const store = new Map<string, Uint8Array>();
  const streams = new Map<number, { bytes: Uint8Array; off: number }>();
  let nextId = 1;
  const cancels: number[] = [];

  const native: NativeStorage = {
    async put(_b, key, b64, _ct) {
      const bytes = Uint8Array.from(Buffer.from(b64, "base64"));
      store.set(key, bytes);
      return JSON.stringify({ bucket: _b, key, size: bytes.length });
    },
    async get(_b, key) {
      const v = store.get(key);
      if (!v) return "null";
      return JSON.stringify({
        bytesBase64: Buffer.from(v).toString("base64"),
        contentType: null,
        size: v.length,
      });
    },
    async delete() {
      return JSON.stringify({ deleted: true });
    },
    async list() {
      return JSON.stringify({ entries: [], cursor: null });
    },
    async putStream(_b, key, body, _ct) {
      const bytes = await drainStream(body);
      store.set(key, bytes);
      return JSON.stringify({ bucket: _b, key, size: bytes.length });
    },
    async getStream(_b, key) {
      const v = store.get(key);
      if (!v) return "null";
      const id = nextId++;
      streams.set(id, { bytes: v, off: 0 });
      return JSON.stringify({ streamId: id, contentType: null, size: v.length });
    },
    async readChunk(streamId) {
      const s = streams.get(streamId);
      if (!s) return undefined;
      if (s.off >= s.bytes.length) {
        streams.delete(streamId);
        return undefined;
      }
      // 64 KiB pull granularity, mirroring a real backend's chunking.
      const end = Math.min(s.off + 64 * 1024, s.bytes.length);
      const chunk = s.bytes.subarray(s.off, end);
      s.off = end;
      return chunk;
    },
    async cancelStream(streamId) {
      cancels.push(streamId);
      streams.delete(streamId);
    },
  };
  return { native, cancels };
}

describe("@zeroship/storage streaming", () => {
  test("put(ReadableStream) routes to putStream and round-trips", async () => {
    const { native } = mockNative();
    const b = new Bucket("uploads", native);

    const payload = new Uint8Array(200 * 1024).map((_, i) => i & 0xff);
    const chunks = [payload.subarray(0, 100 * 1024), payload.subarray(100 * 1024)];

    const putRes = await b.put("big.bin", streamFrom([...chunks]));
    assert.equal(putRes.error, null);
    assert.equal(putRes.data?.size, payload.length);

    const getRes = await b.getStream("big.bin");
    assert.equal(getRes.error, null);
    assert.ok(getRes.data, "getStream should return a handle");
    assert.equal(getRes.data!.size, payload.length);

    const got = await drainStream(getRes.data!.body);
    assert.deepEqual(got, payload);
  });

  test("putStream() direct entry point works", async () => {
    const { native } = mockNative();
    const b = new Bucket("uploads", native);
    const data = new Uint8Array([1, 2, 3, 4, 5]);
    const res = await b.putStream("k", streamFrom([data]));
    assert.equal(res.error, null);
    assert.equal(res.data?.size, 5);
    const back = await drainStream((await b.getStream("k")).data!.body);
    assert.deepEqual(back, data);
  });

  test("getStream of a missing key returns null", async () => {
    const { native } = mockNative();
    const b = new Bucket("uploads", native);
    const res = await b.getStream("nope");
    assert.equal(res.error, null);
    assert.equal(res.data, null);
  });

  test("cancelling the body stream calls cancelStream", async () => {
    const { native, cancels } = mockNative();
    const b = new Bucket("uploads", native);
    await b.putStream("k", streamFrom([new Uint8Array(128 * 1024)]));

    const res = await b.getStream("k");
    const reader = res.data!.body.getReader();
    await reader.read(); // pull the first chunk
    await reader.cancel(); // should propagate to native.cancelStream

    assert.equal(cancels.length, 1, "cancel should reach the native layer exactly once");
  });

  test("small in-memory put still uses the buffered path", async () => {
    let putStreamCalls = 0;
    const { native } = mockNative();
    const wrapped: NativeStorage = {
      ...native,
      async putStream(...args) {
        putStreamCalls++;
        return native.putStream(...args);
      },
    };
    const b = new Bucket("uploads", wrapped);
    await b.put("small.txt", "hello");
    assert.equal(putStreamCalls, 0, "string body must go through buffered put, not putStream");
  });
});

// ---------------------------------------------------------------------------
// list pagination
// ---------------------------------------------------------------------------

// A native `list` over a fixed key set that honours `cursor` + `limit` the
// way both Rust backends do: ascending key order, resume strictly after the
// cursor, `cursor: null` iff the listing is complete.
function pagingNative(keys: string[]): NativeStorage & { calls: number } {
  const sorted = [...keys].sort();
  const stub = {
    calls: 0,
    async list(_b: string, prefix = "", opts: { cursor?: string; limit?: number } = {}) {
      stub.calls++;
      const limit = opts.limit ?? 1000;
      const matching = sorted.filter(
        (k) => k.startsWith(prefix) && (opts.cursor === undefined || k > opts.cursor),
      );
      const entries = matching.slice(0, limit).map((k) => ({ key: k, size: 1, modifiedAt: 0 }));
      const more = matching.length > limit;
      return JSON.stringify({
        entries,
        cursor: more ? entries[entries.length - 1]!.key : null,
      });
    },
  } as unknown as NativeStorage & { calls: number };
  return stub;
}

describe("@zeroship/storage list pagination", () => {
  test("a truncated page reports a cursor and a complete one reports null", async () => {
    const native = pagingNative(["a", "b", "c"]);
    const b = new Bucket("uploads", native);

    const first = await b.list("", { limit: 2 });
    assert.deepEqual(first.data!.entries.map((e) => e.key), ["a", "b"]);
    assert.equal(first.data!.cursor, "b", "a truncated listing must say so");

    const second = await b.list("", { limit: 2, cursor: first.data!.cursor! });
    assert.deepEqual(second.data!.entries.map((e) => e.key), ["c"]);
    assert.equal(second.data!.cursor, null, "an exhausted listing must report cursor null");
  });

  test("listAll pages lazily and stops fetching when the caller breaks", async () => {
    const native = pagingNative(["k1", "k2", "k3", "k4", "k5", "k6"]);
    const b = new Bucket("uploads", native);

    const all: string[] = [];
    for await (const e of b.listAll("", { limit: 2 })) all.push(e.key);
    assert.deepEqual(all, ["k1", "k2", "k3", "k4", "k5", "k6"]);
    // 3 full pages + 1 that reports the end.
    assert.equal(native.calls, 3, "listAll must page, not re-list");

    native.calls = 0;
    const firstTwo: string[] = [];
    for await (const e of b.listAll("", { limit: 2 })) {
      firstTwo.push(e.key);
      if (firstTwo.length === 2) break;
    }
    assert.deepEqual(firstTwo, ["k1", "k2"]);
    assert.equal(native.calls, 1, "breaking out must not fetch the remaining pages");
  });

  test("listAll throws rather than ending the iteration on a failed page", async () => {
    const native = {
      async list() {
        throw new Error("backend exploded");
      },
    } as unknown as NativeStorage;
    const b = new Bucket("uploads", native);
    await assert.rejects(async () => {
      for await (const _ of b.listAll()) { /* unreachable */ }
    }, /backend exploded/);
  });
});
