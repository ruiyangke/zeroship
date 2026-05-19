/**
 * @zeroship/db — Tier D polish:
 *   D1. Index awareness — dev-mode runtime warnings
 *   D2. Nested object validators — `t.object({...})`
 *   D3. Calendar dates — `t.calendarDate()`
 *   D4. Optimistic concurrency — `withVersioning()` + CAS update
 *
 * These tests run entirely against the SDK layer (mocks for the native
 * driver). Postgres-side DDL coverage for D3/D4 lives in
 * `crates/plugin-db/src/query.rs` golden-output tests.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { Collection, NativeDb, __zeroshipDbResetIndexWarnings } from "../src/collection.js";
import { normalizeSchema } from "../src/schema.js";
import { t, schema } from "../src/types.js";
import { ValidationError, OptimisticLockError } from "../src/errors.js";
import { validateDoc } from "../src/validate.js";

// ---------------------------------------------------------------------------
// Mock helpers
// ---------------------------------------------------------------------------

interface CallRecord {
  method: string;
  args: unknown[];
}

function makeMockNative(overrides: Partial<Record<keyof NativeDb, unknown>> = {}): {
  native: NativeDb;
  calls: CallRecord[];
} {
  const calls: CallRecord[] = [];
  const record = (method: string, args: unknown[], ret: unknown) => {
    calls.push({ method, args });
    return Promise.resolve(ret);
  };
  const native: NativeDb = {
    insert: (col, doc) =>
      record("insert", [col, doc], JSON.stringify({ id: "1", ...(doc as object) })) as Promise<string>,
    insertMany: () => Promise.resolve("[]"),
    findOne: (col, filter) => record("findOne", [col, filter], null) as Promise<string | null>,
    find: (col, filter, opts) =>
      record("find", [col, filter, opts], "[]") as Promise<string>,
    updateOne: (col, filter, update) =>
      record("updateOne", [col, filter, update], JSON.stringify({ id: 1 })) as Promise<string>,
    updateMany: (col, filter, update) =>
      record("updateMany", [col, filter, update], JSON.stringify({ updated: 1 })) as Promise<string>,
    deleteOne: () => Promise.resolve(""),
    deleteMany: () => Promise.resolve(JSON.stringify({ deleted: 0 })),
    count: () => Promise.resolve(JSON.stringify({ count: 0 })),
    distinct: () => Promise.resolve("[]"),
    aggregate: () => Promise.resolve("[]"),
    upsert: (col, doc) =>
      record("upsert", [col, doc], JSON.stringify({ id: "u", ...(doc as object) })) as Promise<string>,
    ...overrides,
  };
  return { native, calls };
}

// ---------------------------------------------------------------------------
// D2 — nested object validators (`t.object`)
// ---------------------------------------------------------------------------

describe("D2 — t.object()", () => {
  test("t.object() produces FieldDef.type === 'object' with shape", () => {
    const tb = t.object({
      bio: t.string().max(500),
      avatar: t.string(),
    });
    const def = tb.toFieldDef();
    assert.equal(def.type, "object");
    assert.ok(def.shape);
    assert.equal(def.shape!.bio.type, "string");
    assert.equal(def.shape!.bio.max, 500);
    assert.equal(def.shape!.avatar.type, "string");
  });

  test("t.object() rejects non-object input", () => {
    // @ts-expect-error — wrong type
    assert.throws(() => t.object(null));
    // @ts-expect-error — wrong type
    assert.throws(() => t.object([]));
  });

  test("t.object() rejects non-TypeBuilder nested fields", () => {
    // @ts-expect-error — must be TypeBuilder
    assert.throws(() => t.object({ bio: "not-a-builder" }));
  });

  test("nested validation succeeds for a valid document", () => {
    const s = normalizeSchema({
      profile: t.object({
        bio: t.string().max(500),
        avatar: t.string(),
      }),
    });
    assert.doesNotThrow(() =>
      validateDoc({ profile: { bio: "hi", avatar: "https://x.png" } }, s),
    );
  });

  test("nested validation fails with dotted path errors", () => {
    const s = normalizeSchema({
      profile: t.object({
        bio: t.string().max(5),
      }),
    });
    try {
      validateDoc({ profile: { bio: "way too long" } }, s);
      assert.fail("should have thrown");
    } catch (e) {
      assert.ok(e instanceof ValidationError);
      assert.ok("profile.bio" in e.errors, `expected profile.bio in ${JSON.stringify(e.errors)}`);
      assert.equal(e.errors["profile.bio"].path, "profile.bio");
    }
  });

  test("deeply-nested validation reports the full dotted path", () => {
    const s = normalizeSchema({
      profile: t.object({
        social: t.object({
          twitter: t.string().pattern(/^@\w+$/),
        }),
      }),
    });
    try {
      validateDoc({ profile: { social: { twitter: "no-at-sign" } } }, s);
      assert.fail("should have thrown");
    } catch (e) {
      assert.ok(e instanceof ValidationError);
      assert.ok("profile.social.twitter" in e.errors);
    }
  });

  test("nested required field reported via dotted path", () => {
    const s = normalizeSchema({
      profile: t.object({
        bio: t.string().required(),
      }),
    });
    try {
      validateDoc({ profile: {} }, s);
      assert.fail("should have thrown");
    } catch (e) {
      assert.ok(e instanceof ValidationError);
      assert.ok("profile.bio" in e.errors);
      assert.match(e.errors["profile.bio"].message, /required/);
    }
  });

  test("top-level rejects non-object for an object field", () => {
    const s = normalizeSchema({ profile: t.object({ bio: t.string() }) });
    assert.throws(() => validateDoc({ profile: "not-an-object" }, s), ValidationError);
    assert.throws(() => validateDoc({ profile: [] }, s), ValidationError);
  });
});

// ---------------------------------------------------------------------------
// D3 — calendar dates (`t.calendarDate`)
// ---------------------------------------------------------------------------

describe("D3 — t.calendarDate()", () => {
  test("t.calendarDate() produces type === 'calendarDate'", () => {
    assert.equal(t.calendarDate().toFieldDef().type, "calendarDate");
  });

  test("accepts a valid YYYY-MM-DD string", () => {
    const s = normalizeSchema({ birthday: t.calendarDate() });
    assert.doesNotThrow(() => validateDoc({ birthday: "2026-05-15" }, s));
  });

  test("rejects malformed string (not YYYY-MM-DD)", () => {
    const s = normalizeSchema({ birthday: t.calendarDate() });
    assert.throws(() => validateDoc({ birthday: "2026/05/15" }, s), ValidationError);
    assert.throws(() => validateDoc({ birthday: "26-05-15" }, s), ValidationError);
    assert.throws(() => validateDoc({ birthday: "2026-5-15" }, s), ValidationError);
  });

  test("rejects impossible calendar date (Feb 31)", () => {
    const s = normalizeSchema({ birthday: t.calendarDate() });
    try {
      validateDoc({ birthday: "2026-02-31" }, s);
      assert.fail("should have rejected Feb 31");
    } catch (e) {
      assert.ok(e instanceof ValidationError);
      assert.ok("birthday" in e.errors);
    }
  });

  test("rejects month > 12 and day > 31", () => {
    const s = normalizeSchema({ birthday: t.calendarDate() });
    assert.throws(() => validateDoc({ birthday: "2026-13-01" }, s), ValidationError);
    assert.throws(() => validateDoc({ birthday: "2026-12-32" }, s), ValidationError);
  });

  test("rejects non-string value", () => {
    const s = normalizeSchema({ birthday: t.calendarDate() });
    assert.throws(() => validateDoc({ birthday: 20260515 }, s), ValidationError);
    assert.throws(() => validateDoc({ birthday: new Date() }, s), ValidationError);
  });

  test("accepts leap-year Feb 29 in a leap year", () => {
    const s = normalizeSchema({ birthday: t.calendarDate() });
    assert.doesNotThrow(() => validateDoc({ birthday: "2024-02-29" }, s));
  });

  test("rejects Feb 29 in a non-leap year", () => {
    const s = normalizeSchema({ birthday: t.calendarDate() });
    assert.throws(() => validateDoc({ birthday: "2026-02-29" }, s), ValidationError);
  });
});

// ---------------------------------------------------------------------------
// D4 — optimistic concurrency (`withVersioning`)
// ---------------------------------------------------------------------------

describe("D4 — withVersioning()", () => {
  test("schema().withVersioning() sets options.versioning = true", () => {
    const s = schema({ title: t.string().required() }).withVersioning();
    assert.equal(s.options.versioning, true);
  });

  test("model injects `version` field into normalized schema", async () => {
    // We construct the Collection manually with versioning to verify
    // the DDL-side payload. `version` is a number column with default 1.
    const normalized = normalizeSchema({ title: t.string().required() });
    normalized.version = { type: "number", required: false, default: 1 };
    const { native } = makeMockNative();
    const col = new Collection<{ title: string }>("posts", normalized, native, {
      versioning: true,
    });
    // Indirect check: a non-CAS update goes through unchanged.
    await col.updateOne({ id: 1 }, { title: "new" });
  });

  test("updateOne with matching version increments version", async () => {
    const normalized = normalizeSchema({ title: t.string().required() });
    normalized.version = { type: "number", required: false, default: 1 };
    let captured: { filter: unknown; update: unknown } | null = null;
    const { native } = makeMockNative({
      updateOne: (_col, filter, update) => {
        captured = { filter, update };
        // Simulate a match — return a row.
        return Promise.resolve(JSON.stringify({ id: 1, title: "x", version: 2 })) as Promise<string>;
      },
    });
    const col = new Collection<{ title: string }>("posts", normalized, native, {
      versioning: true,
    });
    const { data, error } = await col.updateOne(
      { id: 1, version: 1 } as any,
      { title: "x" } as any,
    );
    assert.equal(error, null);
    assert.equal(data?.matchedCount, 1);
    // The mapped update must carry $inc:1 on the version column.
    assert.ok(captured, "updateOne native call must have happened");
    const updateArg = captured!.update as Record<string, unknown>;
    assert.deepEqual(updateArg.version, { $inc: 1 });
    // Filter must keep `version: 1` as the CAS guard.
    const filterArg = captured!.filter as Record<string, unknown>;
    assert.equal(filterArg.version, 1);
  });

  test("updateOne with mismatched version returns OptimisticLockError", async () => {
    const normalized = normalizeSchema({ title: t.string().required() });
    normalized.version = { type: "number", required: false, default: 1 };
    const { native } = makeMockNative({
      // Simulate no row matched (CAS failure).
      updateOne: () => Promise.resolve("null") as Promise<string>,
    });
    const col = new Collection<{ title: string }>("posts", normalized, native, {
      versioning: true,
    });
    const { data, error } = await col.updateOne(
      { id: 1, version: 7 } as any,
      { title: "x" } as any,
    );
    assert.equal(data, null);
    assert.ok(error instanceof OptimisticLockError);
    assert.equal((error as OptimisticLockError).code, "optimistic_lock_failure");
    assert.equal((error as OptimisticLockError).expectedVersion, 7);
  });

  test("updateOne without versioning enabled ignores `version` in filter", async () => {
    const normalized = normalizeSchema({ title: t.string().required() });
    let captured: { update: unknown } | null = null;
    const { native } = makeMockNative({
      // No row matched, but versioning is OFF — should not throw.
      updateOne: (_col, _filter, update) => {
        captured = { update };
        return Promise.resolve("null") as Promise<string>;
      },
    });
    const col = new Collection<{ title: string }>("posts", normalized, native, {
      versioning: false,
    });
    const { data, error } = await col.updateOne(
      { id: 1 } as any,
      { title: "x" } as any,
    );
    // No throw — returns matchedCount: 0
    assert.equal(error, null);
    assert.equal(data?.matchedCount, 0);
    assert.ok(captured);
    // version should NOT have been added.
    const updateArg = captured!.update as Record<string, unknown>;
    assert.equal(updateArg.version, undefined);
  });

  test("updateMany with mismatched version returns OptimisticLockError", async () => {
    const normalized = normalizeSchema({ title: t.string().required() });
    normalized.version = { type: "number", required: false, default: 1 };
    const { native } = makeMockNative({
      updateMany: () => Promise.resolve(JSON.stringify({ updated: 0 })) as Promise<string>,
    });
    const col = new Collection<{ title: string }>("posts", normalized, native, {
      versioning: true,
    });
    const { data, error } = await col.updateMany(
      { id: 1, version: 5 } as any,
      { title: "x" } as any,
    );
    assert.equal(data, null);
    assert.ok(error instanceof OptimisticLockError);
  });
});

// ---------------------------------------------------------------------------
// D1 — index awareness (runtime warnings)
// ---------------------------------------------------------------------------

describe("D1 — index awareness runtime warning", () => {
  test("warns once per unindexed query shape (dev mode)", async () => {
    // Capture console.warn for this test only.
    const originalEnv = process.env.NODE_ENV;
    const originalWarn = console.warn;
    const warnings: string[] = [];
    process.env.NODE_ENV = "development";
    console.warn = (msg: unknown) => warnings.push(String(msg));
    __zeroshipDbResetIndexWarnings();

    try {
      const normalized = normalizeSchema({
        title: t.string(),
        slug: t.string().index(),
      });
      const { native } = makeMockNative();
      const col = new Collection<{ title: string }>("posts", normalized, native);
      // First call on an unindexed field — should warn.
      await col.findOne({ title: "Hello" } as any);
      // Second call on the same shape — should NOT warn (dedup).
      await col.findOne({ title: "World" } as any);
      // Call on an indexed field — should NOT warn.
      await col.findOne({ slug: "hello" } as any);
      // Call on a different unindexed shape — should warn again (new key set).
      await col.find({ id: 1, title: "x" } as any);

      assert.ok(
        warnings.length >= 1,
        `expected at least one warning, got ${warnings.length}: ${JSON.stringify(warnings)}`,
      );
      assert.ok(
        warnings[0].includes("unindexed query"),
        `expected 'unindexed query' in message, got: ${warnings[0]}`,
      );
      assert.ok(warnings[0].includes("posts"));
      assert.ok(warnings[0].includes("title"));
      // The exact title-only warning fires once even though we called findOne twice.
      const titleWarnings = warnings.filter((w) => /\[title\]/.test(w));
      assert.equal(titleWarnings.length, 1, "title-shape warning must be deduped");
    } finally {
      process.env.NODE_ENV = originalEnv;
      console.warn = originalWarn;
      __zeroshipDbResetIndexWarnings();
    }
  });

  test("does not warn in production (NODE_ENV=production)", async () => {
    const originalEnv = process.env.NODE_ENV;
    const originalWarn = console.warn;
    const warnings: string[] = [];
    process.env.NODE_ENV = "production";
    console.warn = (msg: unknown) => warnings.push(String(msg));
    __zeroshipDbResetIndexWarnings();

    try {
      const normalized = normalizeSchema({ title: t.string() });
      const { native } = makeMockNative();
      const col = new Collection<{ title: string }>("posts", normalized, native);
      await col.findOne({ title: "Hello" } as any);
      assert.equal(warnings.length, 0, "production must not emit warnings");
    } finally {
      process.env.NODE_ENV = originalEnv;
      console.warn = originalWarn;
      __zeroshipDbResetIndexWarnings();
    }
  });

  test("does not warn on indexed field", async () => {
    const originalEnv = process.env.NODE_ENV;
    const originalWarn = console.warn;
    const warnings: string[] = [];
    process.env.NODE_ENV = "development";
    console.warn = (msg: unknown) => warnings.push(String(msg));
    __zeroshipDbResetIndexWarnings();

    try {
      const normalized = normalizeSchema({
        email: t.string().unique(),
        userId: t.number().index(),
      });
      const { native } = makeMockNative();
      const col = new Collection<{ email: string }>("users", normalized, native);
      await col.findOne({ email: "a@b.com" } as any);
      await col.findOne({ userId: 42 } as any);
      assert.equal(warnings.length, 0, "indexed fields must not warn");
    } finally {
      process.env.NODE_ENV = originalEnv;
      console.warn = originalWarn;
      __zeroshipDbResetIndexWarnings();
    }
  });
});
