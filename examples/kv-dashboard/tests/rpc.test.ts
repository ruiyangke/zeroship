import { expect, test } from "vitest";
import { setTimeout as sleep } from "node:timers/promises";
import { listKeys, rpc, type Row } from "./rpc";
import { targets } from "./targets";

type Capture = Record<string, Row>;

function validate(capture: Capture) {
  expect(capture.visit1).toMatchObject({ visits: 1 });
  expect(capture.visit2).toMatchObject({ visits: 2 });
  expect(capture.flag).toMatchObject({ checkoutEnabled: true });
  for (let count = 1; count <= 6; count++) {
    expect(capture[`rate${count}`]).toMatchObject({ count, allowed: count <= 5, remaining: Math.max(0, 5 - count) });
  }
  const ttl = (value: unknown, low: number, high: number) => {
    expect(value).toBeTypeOf("number");
    expect(value).toBeGreaterThan(low);
    expect(value).toBeLessThanOrEqual(high);
  };
  ttl(capture.rate6.resetMs, 0, 60_000);
  for (const [first, second, field] of [["cache1", "cache2", "quote"], ["memo1", "memo2", "value"]]) {
    expect(capture[first].source).toBe("miss");
    expect(capture[second].source).toBe("hit");
    expect(capture[first][field]).toBeTypeOf("object");
    expect(capture[first][field]).not.toBeNull();
    expect(capture[second][field]).toStrictEqual(capture[first][field]);
    ttl(capture[second].ttlMs, 0, 30_000);
  }
  expect(capture.lease1).toMatchObject({ acquired: true, lease: { owner: "owner-a" } });
  expect(capture.lease2).toMatchObject({ acquired: false, lease: { owner: "owner-a" } });
  expect(capture.leaseClear.lease).toBeNull();
  expect(capture.lease3).toMatchObject({ acquired: true, lease: { owner: "owner-b" } });
  expect(capture.stringSet).toMatchObject({ value: "hello probe", has: true });
  ttl(capture.stringSet.ttlMs, 55_000, 60_000);
  expect(capture.snapshotSet).toMatchObject({ text: { value: "hello probe", has: true } });
  expect(capture.stringExpire.updated).toBe(true);
  ttl(capture.stringExpire.ttlMs, 115_000, 120_000);
  expect(capture.stringPersist).toMatchObject({ updated: true, ttlMs: null });
  expect(capture.stringDelete).toMatchObject({ deleted: true, value: null, has: false });
  expect(capture.snapshotDelete).toMatchObject({ text: { value: null, has: false } });
  expect(capture.keys.keys).toEqual(expect.arrayContaining([
    "kv-demo:counter:visits", "kv-demo:cache:quote:probe-sku", "kv-demo:memo:probe-memo",
  ]));
  expect(capture.keys.keys).not.toContain("kv-demo:strings:greeting");
  expect(capture.keys.keys).not.toContain("kv-demo:leases:deploy");
}

function normalize(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(normalize);
  if (value && typeof value === "object") {
    return Object.fromEntries(Object.entries(value).map(([key, entry]) => {
      if (["resetMs", "ttlMs", "expiresAt", "generatedAt", "builtAt", "createdAt", "acquiredAt", "nonce", "token", "leaseId", "price"].includes(key) && entry !== null) {
        return [key, "<volatile>"];
      }
      const normalized = normalize(entry);
      return [key, key === "keys" && Array.isArray(normalized) ? normalized.sort() : normalized];
    }));
  }
  if (typeof value === "string" && value.startsWith("kv-demo:rate:")) return value.replace(/:[^:]+$/, ":<window>");
  return value;
}

test("the public KV contract holds on every configured backend", async () => {
  const captures: Capture[] = [];
  for (const target of targets()) {
    console.info(`KV RPC: ${target.name}`);
    const call = (operation: string, input: Row = {}) => rpc(target.apiUrl, operation, input);
    await call("kv.clear");
    try {
      const capture: Capture = {};
      const row = async (name: string, operation: string, input: Row = {}) => {
        capture[name] = await call(operation, input);
      };
      await row("visit1", "kv.visit");
      await row("visit2", "kv.visit");
      await row("flag", "kv.flag.set", { enabled: true });
      const remaining = 60_000 - Date.now() % 60_000;
      if (remaining < 5_000) await sleep(remaining + 1);
      for (let i = 1; i <= 6; i++) await row(`rate${i}`, "kv.rate.hit", { actor: "probe" });
      for (let i = 1; i <= 2; i++) await row(`cache${i}`, "kv.cache.quote", { sku: "probe-sku" });
      for (let i = 1; i <= 2; i++) await row(`memo${i}`, "kv.memo.get", { label: "probe-memo" });
      await row("lease1", "kv.lease.acquire", { owner: "owner-a" });
      await row("lease2", "kv.lease.acquire", { owner: "owner-b" });
      await row("leaseClear", "kv.lease.clear");
      await row("lease3", "kv.lease.acquire", { owner: "owner-b" });
      await call("kv.lease.clear");
      await row("stringSet", "kv.string.set", { value: "hello probe", ttlMs: 60_000 });
      await row("snapshotSet", "kv.snapshot");
      await row("stringExpire", "kv.string.expire", { ttlMs: 120_000 });
      await row("stringPersist", "kv.string.persist");
      await row("stringDelete", "kv.string.delete");
      await row("snapshotDelete", "kv.snapshot");
      const session = await call("kv.session.create", { name: "Smoke User" });
      expect(session.token).toBeTypeOf("string");
      expect(session.token).not.toBe("");
      expect(await listKeys(target.apiUrl, "session:")).toContain(`kv-demo:session:${session.token}`);
      expect(await call("kv.session.delete", { token: session.token })).toMatchObject({ deleted: true });
      expect(await listKeys(target.apiUrl, "session:")).toEqual([]);
      capture.keys = { keys: await listKeys(target.apiUrl) };
      validate(capture);

      // Prove that comparison normalization cannot hide broken behavior.
      for (const [label, field, value] of [
        ["rate6", "allowed", true], ["cache2", "quote", { price: -1 }],
        ["memo2", "value", { nonce: "recomputed" }], ["stringSet", "ttlMs", 1_000],
        ["stringDelete", "has", true], ["keys", "keys", []],
      ] as const) {
        const damaged = structuredClone(capture);
        damaged[label][field] = value;
        expect(() => validate(damaged), `${label}.${field} must be checked`).toThrow();
      }
      const missing = structuredClone(capture);
      delete missing.stringDelete;
      expect(() => validate(missing)).toThrow();
      captures.push(capture);
    } finally {
      await call("kv.clear");
    }
  }
  for (const capture of captures.slice(1)) expect(normalize(capture)).toStrictEqual(normalize(captures[0]));
});
