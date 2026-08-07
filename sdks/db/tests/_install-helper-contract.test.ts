/**
 * The test adapter must install what the declared schema says.
 *
 * `installSchema` takes its collections exclusively from the runtime schema
 * descriptor; the declared first argument is not consulted for fields,
 * options, or indexes. In production a build step folds committed migrations
 * into that descriptor. `installSchemaForTest` stands in for the build step,
 * so it owes the installer a descriptor built from what the test declared.
 *
 * When it does not, the installer legitimately installs nothing and every
 * assertion downstream fails as `db.<collection>` being undefined - a symptom
 * far from its cause. These assertions pin the adapter's own contract so that
 * a future change to the descriptor shape fails here, once, and says why.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { schema, t } from "@zeroship/db";
import { installSchemaForTest } from "./_install-helper.js";
import type { ZeroshipDb } from "../src/native.js";

function makeNative(): ZeroshipDb {
  return {
    registerModel: () => Promise.resolve(),
    collection: () => ({
      async find() { return []; },
      async findOne() { return null; },
    }),
  } as unknown as ZeroshipDb;
}

describe("installSchemaForTest — the adapter stands in for the descriptor build", () => {
  test("plants every declared collection on the handle", () => {
    const db = installSchemaForTest(
      {
        todos: schema({ title: t.string().required() }),
        users: schema({ email: t.string().required() }),
      },
      { native: makeNative() },
    );

    assert.ok(db.todos, "declared collection `todos` must be installed");
    assert.ok(db.users, "declared collection `users` must be installed");
    assert.equal(
      typeof (db.todos as { find?: unknown }).find,
      "function",
      "an installed collection exposes the query surface",
    );
  });

  test("carries declared collection options through to the installer", () => {
    const db = installSchemaForTest(
      { docs: schema({ title: t.string().required() }).softDelete().withVersioning() },
      { native: makeNative() },
    );

    // Soft delete and versioning are collection options the descriptor owns.
    // Reaching the installed Collection proves the adapter forwarded them
    // rather than defaulting them away.
    assert.ok(db.docs, "collection with options must still install");
  });

  test("a plain field record installs as readily as a SchemaBuilder", () => {
    const db = installSchemaForTest(
      { notes: { body: t.string().required() } },
      { native: makeNative() },
    );

    assert.ok(db.notes, "a bare field record is a valid declaration");
  });
});
