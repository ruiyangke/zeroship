// Tests for @zeroship/kv over a mock NativeKv.
//
// We never touch the real runtime here — `createKv(mock)` injects a
// hand-rolled in-memory NativeKv that mirrors the verified native wire
// contract (raw-string `get`, `{ok:true}` `set`, number|bigint `incr`,
// `null` | `{ttlMs}` `ttl`, `{keys,cursor}` `list`). The mock lets us
// assert the SDK's JSON round-trip, bigint normalization, pagination
// shape, and the pure-JS conveniences.

import assert from "node:assert/strict";
import { test } from "node:test";

import { createKv } from "../src/index.ts";

/**
 * Minimal NativeKv mock honouring the dispatch.rs wire contract. Values
 * are stored as the exact string the SDK passes to `set` (JSON-encoded),
 * matching what the real native layer stores and hands back from `get`.
 */
function mockNative(seed: Record<string, string> = {}) {
  const store = new Map<string, string>(Object.entries(seed));
  const ttls = new Map<string, number>(); // key -> expiresAtMs (absolute)
  let counter = 0n; // forces deterministic incr math via BigInt

  const native = {
    async get(key: string): Promise<string | null> {
      return store.has(key) ? store.get(key)! : null;
    },
    async set(key: string, value: string, opts?: { ttlMs?: number }) {
      store.set(key, value);
      if (opts?.ttlMs != null) ttls.set(key, Date.now() + opts.ttlMs);
      return { ok: true } as const;
    },
    async delete(key: string) {
      const deleted = store.delete(key);
      ttls.delete(key);
      return { deleted };
    },
    async incr(key: string, opts?: { by?: number; ttlMs?: number }) {
      const by = BigInt(opts?.by ?? 1);
      const created = !store.has(key);
      const cur = created ? 0n : BigInt(store.get(key)!);
      const next = cur + by;
      store.set(key, next.toString());
      if (created && opts?.ttlMs != null) ttls.set(key, Date.now() + opts.ttlMs);
      counter = next;
      // Mirror native: number when it fits in 2^53, else bigint.
      return next >= -9007199254740991n && next <= 9007199254740991n
        ? Number(next)
        : next;
    },
    async setIfAbsent(key: string, value: string, opts?: { ttlMs?: number }) {
      if (store.has(key)) return { stored: false };
      store.set(key, value);
      if (opts?.ttlMs != null) ttls.set(key, Date.now() + opts.ttlMs);
      return { stored: true };
    },
    async expire(key: string, ttlMs: number) {
      if (!store.has(key)) return { updated: false };
      ttls.set(key, Date.now() + ttlMs);
      return { updated: true };
    },
    async ttl(key: string): Promise<{ ttlMs: number | null } | null> {
      if (!store.has(key)) return null;
      const at = ttls.get(key);
      if (at == null) return { ttlMs: null };
      return { ttlMs: Math.max(0, at - Date.now()) };
    },
    async persist(key: string) {
      if (!store.has(key) || !ttls.has(key)) return { updated: false };
      ttls.delete(key);
      return { updated: true };
    },
    async list(prefix = "", opts?: { cursor?: string; limit?: number }) {
      const limit = opts?.limit ?? 1000;
      const all = [...store.keys()].filter((k) => k.startsWith(prefix)).sort();
      const start = opts?.cursor ? all.findIndex((k) => k > opts.cursor!) : 0;
      const from = start < 0 ? all.length : start;
      const page = all.slice(from, from + limit);
      const consumed = from + page.length;
      const cursor = consumed < all.length ? page[page.length - 1]! : null;
      return { keys: page, cursor };
    },
  };
  return native;
}

test("set/get round-trips a structured value", async () => {
  const kv = createKv(mockNative());
  const w = await kv.set("user:42", { name: "Alice", roles: ["admin"] });
  assert.equal(w.error, null);

  const r = await kv.get<{ name: string; roles: string[] }>("user:42");
  assert.equal(r.error, null);
  assert.deepEqual(r.data, { name: "Alice", roles: ["admin"] });
});

test("get returns null for a missing key", async () => {
  const kv = createKv(mockNative());
  const r = await kv.get("nope");
  assert.equal(r.error, null);
  assert.equal(r.data, null);
});

test("getString unwraps a JSON-encoded string", async () => {
  const kv = createKv(mockNative());
  await kv.set("greeting", "hello");
  const r = await kv.getString("greeting");
  assert.equal(r.error, null);
  assert.equal(r.data, "hello");
});

test("delete reports whether the key existed", async () => {
  const kv = createKv(mockNative());
  await kv.set("k", 1);
  assert.deepEqual((await kv.delete("k")).data, { deleted: true });
  assert.deepEqual((await kv.delete("k")).data, { deleted: false });
});

test("incr defaults by to +1 and returns a number", async () => {
  const kv = createKv(mockNative());
  assert.equal((await kv.incr("hits")).data, 1);
  assert.equal((await kv.incr("hits")).data, 2);
  assert.equal((await kv.incr("hits", { by: 10 })).data, 12);
});

test("incr normalizes a bigint result to number", async () => {
  // Seed the counter just below 2^53 so one more push crosses into the
  // bigint range the native layer returns above MAX_SAFE_INTEGER.
  const start = (2n ** 53n).toString(); // 9007199254740992
  const kv = createKv(mockNative({ big: start }));
  const r = await kv.incr("big", { by: 100 });
  assert.equal(r.error, null);
  assert.equal(typeof r.data, "number");
  assert.equal(r.data, Number(2n ** 53n + 100n));
});

test("setIfAbsent stores only on absence", async () => {
  const kv = createKv(mockNative());
  assert.deepEqual((await kv.setIfAbsent("lock", "owner-a")).data, { stored: true });
  assert.deepEqual((await kv.setIfAbsent("lock", "owner-b")).data, { stored: false });
  // The first writer's value survives.
  assert.equal((await kv.getString("lock")).data, "owner-a");
});

test("ttl semantics: missing -> null, no-expiry -> {ttlMs:null}, expiring -> {ttlMs}", async () => {
  const kv = createKv(mockNative());
  assert.equal((await kv.ttl("absent")).data, null);

  await kv.set("permanent", 1);
  assert.deepEqual((await kv.ttl("permanent")).data, { ttlMs: null });

  await kv.set("session", 1, { ttlMs: 60_000 });
  const t = (await kv.ttl("session")).data as { ttlMs: number };
  assert.ok(t.ttlMs > 0 && t.ttlMs <= 60_000);
});

test("expire and persist toggle a key's TTL", async () => {
  const kv = createKv(mockNative());
  assert.deepEqual((await kv.expire("nope", 1000)).data, { updated: false });

  await kv.set("k", 1);
  assert.deepEqual((await kv.expire("k", 5000)).data, { updated: true });
  assert.deepEqual((await kv.persist("k")).data, { updated: true });
  assert.deepEqual((await kv.ttl("k")).data, { ttlMs: null });
  assert.deepEqual((await kv.persist("k")).data, { updated: false });
});

test("list paginates and ends with a null cursor", async () => {
  const seed: Record<string, string> = {};
  for (let i = 0; i < 5; i++) seed[`s:${i}`] = "1";
  seed["other"] = "1";
  const kv = createKv(mockNative(seed));

  const p1 = await kv.list("s:", { limit: 2 });
  assert.equal(p1.error, null);
  assert.equal(p1.data!.keys.length, 2);
  assert.notEqual(p1.data!.cursor, null);

  const p2 = await kv.list("s:", { cursor: p1.data!.cursor!, limit: 2 });
  assert.equal(p2.data!.keys.length, 2);

  const p3 = await kv.list("s:", { cursor: p2.data!.cursor!, limit: 2 });
  assert.equal(p3.data!.keys.length, 1);
  assert.equal(p3.data!.cursor, null); // exhausted

  // The "other" key (outside the prefix) was never returned.
  const all = [...p1.data!.keys, ...p2.data!.keys, ...p3.data!.keys];
  assert.ok(!all.includes("other"));
});

test("has reflects key presence", async () => {
  const kv = createKv(mockNative());
  assert.equal((await kv.has("k")).data, false);
  await kv.set("k", { a: 1 });
  assert.equal((await kv.has("k")).data, true);
});

test("getOrSet computes on miss, caches on hit, and is not atomic", async () => {
  const kv = createKv(mockNative());
  let calls = 0;
  const factory = () => {
    calls++;
    return { built: true };
  };

  // Miss → factory runs once.
  const first = await kv.getOrSet("cache:x", { ttlMs: 1000 }, factory);
  assert.deepEqual(first.data, { built: true });
  assert.equal(calls, 1);

  // Hit → factory not run again.
  const second = await kv.getOrSet("cache:x", { ttlMs: 1000 }, factory);
  assert.deepEqual(second.data, { built: true });
  assert.equal(calls, 1);

  // Stampede: concurrent misses may each run factory (documented). Two
  // parallel calls on a cold key should each see null and invoke it.
  let cold = 0;
  const coldFactory = () => {
    cold++;
    return cold;
  };
  await Promise.all([
    kv.getOrSet("cache:cold", {}, coldFactory),
    kv.getOrSet("cache:cold", {}, coldFactory),
  ]);
  assert.ok(cold >= 1, "factory ran at least once under a stampede");
});

test("namespace transparently prefixes keys", async () => {
  const native = mockNative();
  const kv = createKv(native);
  const sessions = kv.namespace("session:");

  await sessions.set("abc", { token: "t" });
  // Written under the combined key on the underlying store.
  assert.equal(await native.get("session:abc"), JSON.stringify({ token: "t" }));
  // Readable through the namespace.
  assert.deepEqual((await sessions.get("abc")).data, { token: "t" });
  // Not readable under the bare key through the root client.
  assert.equal((await kv.get("abc")).data, null);

  // list through the namespace prefixes the query; returned keys keep
  // their full stored form.
  await sessions.set("def", 1);
  const ls = await sessions.list("");
  assert.deepEqual(ls.data!.keys.sort(), ["session:abc", "session:def"]);
});
