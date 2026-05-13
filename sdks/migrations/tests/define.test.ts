/**
 * Unit tests for `defineMigration`.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { defineMigration } from "../src/define.js";

describe("defineMigration", () => {
  test("returns a frozen descriptor with required fields", () => {
    const m = defineMigration({
      name: "backfill_role",
      collection: "users",
      migrateOne: () => ({ role: "user" }),
    });
    assert.equal(m.name, "backfill_role");
    assert.equal(m.collection, "users");
    assert.equal(typeof m.migrateOne, "function");
    assert.equal(m.batchSize, 200, "default batchSize");
    assert.equal(m.failureBudget, 0, "default failureBudget");
    assert.throws(() => {
      (m as { name: string }).name = "hacked";
    });
  });

  test("respects explicit batchSize and failureBudget", () => {
    const m = defineMigration({
      name: "x",
      collection: "users",
      batchSize: 50,
      failureBudget: 3,
      migrateOne: () => undefined,
    });
    assert.equal(m.batchSize, 50);
    assert.equal(m.failureBudget, 3);
  });

  test("rejects empty name", () => {
    assert.throws(
      () => defineMigration({ name: "", collection: "users", migrateOne: () => undefined }),
      /name/i,
    );
  });

  test("rejects empty collection", () => {
    assert.throws(
      () => defineMigration({ name: "x", collection: "", migrateOne: () => undefined }),
      /collection/i,
    );
  });

  test("rejects non-positive batchSize", () => {
    assert.throws(
      () => defineMigration({ name: "x", collection: "users", batchSize: 0, migrateOne: () => undefined }),
      /batchSize/,
    );
    assert.throws(
      () => defineMigration({ name: "x", collection: "users", batchSize: -1, migrateOne: () => undefined }),
      /batchSize/,
    );
  });

  test("rejects oversize batchSize", () => {
    assert.throws(
      () => defineMigration({ name: "x", collection: "users", batchSize: 10001, migrateOne: () => undefined }),
      /batchSize/,
    );
  });

  test("rejects non-function migrateOne", () => {
    assert.throws(
      () => defineMigration({ name: "x", collection: "users", migrateOne: undefined as unknown as () => undefined }),
      /migrateOne/,
    );
  });
});
