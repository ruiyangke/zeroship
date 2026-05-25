/**
 * **P5 PR 2** — `t.encrypted(...)` builder + SDK filter-validation
 * fence.
 *
 * These tests pin behaviour that the runtime side cannot enforce:
 *
 *   1. The SDK refuses ANY filter on a randomised-encrypted column
 *      (`RANDOMISED_ENCRYPTED_FIELD_NOT_FILTERABLE`).
 *   2. The SDK refuses range / regex / LIKE on a deterministic-
 *      encrypted column (`DETERMINISTIC_ENCRYPTED_OP_NOT_SUPPORTED`).
 *   3. `t.encrypted({ wraps: t.object(...) })` rejects with
 *      `ENCRYPTED_WRAPS_UNSUPPORTED` at schema-definition time.
 *   4. `t.encrypted({ mode: "randomised" }).unique()` rejects with
 *      `UNIQUE_ENCRYPTED_RANDOMISED_UNSUPPORTED`.
 *   5. A bare equality filter on a deterministic column passes.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { t } from "@zeroship/db";
import { installSchemaForTest } from "./_install-helper.js";

type PlainObject = Record<string, unknown>;

function makeMockNative() {
  // Mock native that records calls. We never hit the body for the
  // rejection tests; the fence throws synchronously before any
  // `find`/`updateMany`/etc. native call.
  const noop = async () => null;
  const noopList = async () => [];
  const noopCount = async () => 0;
  const collection = (): unknown => ({
    find: noopList,
    insert: noop,
    insertMany: noopList,
    update: noop,
    updateMany: noopCount,
    delete: noop,
    deleteMany: noopCount,
    upsert: noop,
    count: noopCount,
    distinct: noopList,
    aggregate: noopList,
    search: noopList,
    near: noopList,
  });
  return {
    collection,
    registerModel: async () => null,
  } as unknown as Parameters<typeof installSchemaForTest>[1]["native"];
}

describe("P5 PR 2 — t.encrypted(...) builder", () => {
  test("default mode is randomised, default keyId is 'default', default wraps is string", () => {
    const b = t.encrypted();
    const def = b.toFieldDef();
    assert.equal(def.type, "string");
    assert.ok(def.encrypted);
    assert.equal(def.encrypted!.mode, "randomised");
    assert.equal(def.encrypted!.keyId, "default");
    assert.equal(def.encrypted!.wraps, "string");
  });

  test("mode=deterministic + keyId='pii_v2' + wraps=t.number() round-trips through FieldDef", () => {
    const b = t.encrypted({ mode: "deterministic", keyId: "pii_v2", wraps: t.number() });
    const def = b.toFieldDef();
    assert.equal(def.type, "number");
    assert.equal(def.encrypted!.mode, "deterministic");
    assert.equal(def.encrypted!.keyId, "pii_v2");
    assert.equal(def.encrypted!.wraps, "number");
  });

  test("wraps=t.bytes() yields wraps='bytes' in FieldDef", () => {
    const b = t.encrypted({ wraps: t.bytes() });
    const def = b.toFieldDef();
    assert.equal(def.encrypted!.wraps, "bytes");
  });

  test("wraps=t.boolean() rejects with encrypted_wraps_unsupported", () => {
    assert.throws(
      () => t.encrypted({ wraps: t.boolean() as never }),
      (e: Error & { code?: string }) => e.code === "ENCRYPTED_WRAPS_UNSUPPORTED",
    );
  });

  test("wraps=t.object({...}) rejects with encrypted_wraps_unsupported", () => {
    assert.throws(
      () => t.encrypted({ wraps: t.object({ a: t.string() }) as never }),
      (e: Error & { code?: string }) => e.code === "ENCRYPTED_WRAPS_UNSUPPORTED",
    );
  });

  test("wraps=t.ref('users') rejects with encrypted_wraps_unsupported (P5 Q-P5-I)", () => {
    // Equivalent of "encrypted-on-ref unsupported": the only way to
    // mark a ref as encrypted via the SDK is `t.encrypted({ wraps:
    // t.ref(...) })`; the constructor refuses that combination here.
    // FK columns must stay unencrypted so JOIN integrity works.
    assert.throws(
      () => t.encrypted({ wraps: t.ref("users") as never }),
      (e: Error & { code?: string }) => e.code === "ENCRYPTED_WRAPS_UNSUPPORTED",
    );
  });

  test("invalid mode rejects with encrypted_invalid_mode", () => {
    assert.throws(
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      () => t.encrypted({ mode: "asymmetric" as any }),
      (e: Error & { code?: string }) => e.code === "ENCRYPTED_INVALID_MODE",
    );
  });

  test("invalid keyId rejects with encrypted_invalid_key_id", () => {
    assert.throws(
      () => t.encrypted({ keyId: "has spaces" }),
      (e: Error & { code?: string }) => e.code === "ENCRYPTED_INVALID_KEY_ID",
    );
  });

  test("randomised + .unique() rejects with unique_encrypted_randomised_unsupported", () => {
    assert.throws(
      () => t.encrypted({ mode: "randomised" }).unique(),
      (e: Error & { code?: string }) => e.code === "UNIQUE_ENCRYPTED_RANDOMISED_UNSUPPORTED",
    );
  });

  test("deterministic + .unique() is permitted (equality on ciphertext is sound)", () => {
    const b = t.encrypted({ mode: "deterministic" }).unique();
    assert.equal(b.toFieldDef().unique, true);
    assert.equal(b.toFieldDef().encrypted!.mode, "deterministic");
  });
});

describe("P5 PR 2 — SDK filter validation fence (P5 gate #3 / IMPORTANT #1)", () => {
  function dbWithEncrypted() {
    const native = makeMockNative();
    return installSchemaForTest(
      {
        users: {
          name: t.string().required(),
          ssnRandom: t.encrypted({ mode: "randomised" }),
          ssnDet: t.encrypted({ mode: "deterministic" }),
        },
      },
      { native },
    );
  }

  test("find({ssnRandom: 'X'}) throws randomised_encrypted_field_not_filterable", () => {
    const db = dbWithEncrypted();
    assert.throws(
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      () => (db.users as any).find({ ssnRandom: "X" }),
      (e: Error & { code?: string }) => e.code === "RANDOMISED_ENCRYPTED_FIELD_NOT_FILTERABLE",
    );
  });

  test("find({ssnRandom: {$eq: 'X'}}) throws randomised_encrypted_field_not_filterable", () => {
    const db = dbWithEncrypted();
    assert.throws(
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      () => (db.users as any).find({ ssnRandom: { $eq: "X" } }),
      (e: Error & { code?: string }) => e.code === "RANDOMISED_ENCRYPTED_FIELD_NOT_FILTERABLE",
    );
  });

  test("find({$or: [{ssnRandom: 'X'}, ...]}) recurses into $or arms and throws", () => {
    const db = dbWithEncrypted();
    assert.throws(
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      () => (db.users as any).find({ $or: [{ ssnRandom: "X" }, { name: "y" }] }),
      (e: Error & { code?: string }) => e.code === "RANDOMISED_ENCRYPTED_FIELD_NOT_FILTERABLE",
    );
  });

  test("find({ssnDet: 'X'}) passes — bare value is treated as $eq", () => {
    const db = dbWithEncrypted();
    // No throw expected.
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const q = (db.users as any).find({ ssnDet: "X" });
    assert.ok(q);
  });

  test("find({ssnDet: {$eq: 'X'}}) passes", () => {
    const db = dbWithEncrypted();
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const q = (db.users as any).find({ ssnDet: { $eq: "X" } });
    assert.ok(q);
  });

  test("find({ssnDet: {$in: ['A', 'B']}}) passes", () => {
    const db = dbWithEncrypted();
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const q = (db.users as any).find({ ssnDet: { $in: ["A", "B"] } });
    assert.ok(q);
  });

  test("find({ssnDet: {$gt: 'X'}}) throws deterministic_encrypted_op_not_supported", () => {
    const db = dbWithEncrypted();
    assert.throws(
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      () => (db.users as any).find({ ssnDet: { $gt: "X" } as PlainObject }),
      (e: Error & { code?: string }) => e.code === "DETERMINISTIC_ENCRYPTED_OP_NOT_SUPPORTED",
    );
  });

  test("find({ssnDet: {$like: 'X%'}}) throws deterministic_encrypted_op_not_supported", () => {
    const db = dbWithEncrypted();
    assert.throws(
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      () => (db.users as any).find({ ssnDet: { $like: "X%" } as PlainObject }),
      (e: Error & { code?: string }) => e.code === "DETERMINISTIC_ENCRYPTED_OP_NOT_SUPPORTED",
    );
  });

  test("count({ssnRandom: 'X'}) returns Result.error.code = randomised_encrypted_field_not_filterable", async () => {
    const db = dbWithEncrypted();
    // count is async — the throw inside surfaces synchronously since
    // the validator runs before _run; assert.throws doesn't await, so
    // we wrap and assert on the rejected promise.
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    await assert.rejects(
      async () => {
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        return (db.users as any).count({ ssnRandom: "X" });
      },
      (e: Error & { code?: string }) => e.code === "RANDOMISED_ENCRYPTED_FIELD_NOT_FILTERABLE",
    );
  });

  test("distinct('ssnRandom') throws distinct_on_encrypted_field_unsupported", async () => {
    const db = dbWithEncrypted();
    await assert.rejects(
      async () => {
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        return (db.users as any).distinct("ssnRandom");
      },
      (e: Error & { code?: string }) => e.code === "DISTINCT_ON_ENCRYPTED_FIELD_UNSUPPORTED",
    );
  });

  test("find({name: 'alice'}) on a schema with encrypted columns passes (non-encrypted filter)", () => {
    const db = dbWithEncrypted();
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const q = (db.users as any).find({ name: "alice" });
    assert.ok(q);
  });
});
