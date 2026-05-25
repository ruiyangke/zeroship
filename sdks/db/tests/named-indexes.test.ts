/**
 * Named multi-column indexes — `schema(...).index(name, fields)`.
 *
 * Covers both halves of the feature:
 *   1. Definition-time validation on the SchemaBuilder.
 *   2. Runtime warning suppression: a filter whose keys form a prefix of
 *      a declared index does NOT fire the unindexed-query warning; a
 *      filter that does not match any prefix DOES.
 *
 * The warning path is normally silenced in `NODE_ENV=test` so it doesn't
 * pollute every other suite. We opt in by setting the
 * `__zeroshipDbWarnIndexInTest` global before each warning case.
 */
import { test, describe, beforeEach, afterEach } from "node:test";
import assert from "node:assert/strict";
import { schema, t } from "@zeroship/db";
import { installSchemaForTest } from "./_install-helper.js";
import { __zeroshipDbResetIndexWarnings } from "../src/collection.js";

// ---------------------------------------------------------------------------
// Definition-time API
// ---------------------------------------------------------------------------

describe("SchemaBuilder.index(name, fields) — definition-time validation", () => {
  test("declares a single named index on a known field", () => {
    const s = schema({
      email: t.string().required(),
    }).index("by_email", ["email"]);

    assert.equal(s.indexes.length, 1);
    assert.equal(s.indexes[0].name, "by_email");
    assert.deepEqual(s.indexes[0].fields, ["email"]);
    assert.equal(s.indexes[0].unique, undefined);
  });

  test("declares a multi-column index in declared field order", () => {
    const s = schema({
      userId: t.number().required(),
      done: t.boolean().default(false),
      created_at: t.timestamp(),
    }).index("by_user_done", ["userId", "done"]);

    assert.deepEqual(s.indexes[0].fields, ["userId", "done"]);
  });

  test("chains multiple .index() calls in declaration order", () => {
    const s = schema({
      email: t.string().required(),
      userId: t.number(),
      done: t.boolean(),
    })
      .index("by_email", ["email"])
      .index("by_user_done", ["userId", "done"]);

    assert.equal(s.indexes.length, 2);
    assert.equal(s.indexes[0].name, "by_email");
    assert.equal(s.indexes[1].name, "by_user_done");
  });

  test("accepts the auto-generated columns (id, created_at, updated_at) in fields", () => {
    const s = schema({
      title: t.string().required(),
    }).index("by_recency", ["created_at"]);

    assert.deepEqual(s.indexes[0].fields, ["created_at"]);
  });

  test("uniqueIndex(name, fields) sets unique: true on the spec", () => {
    const s = schema({
      orgId: t.number().required(),
      slug: t.string().required(),
    }).uniqueIndex("by_org_slug", ["orgId", "slug"]);

    assert.equal(s.indexes[0].unique, true);
  });

  test("rejects a duplicate index name at definition time", () => {
    try {
      schema({ email: t.string() })
        .index("by_email", ["email"])
        .index("by_email", ["email"]);
      assert.fail("expected throw on duplicate index name");
    } catch (e) {
      const err = e as Error & { code?: string };
      assert.equal(err.code, "SCHEMA_INVALID");
      assert.match(err.message, /already declared/);
    }
  });

  test("rejects a field that is not declared on the schema", () => {
    try {
      schema({ email: t.string() }).index("by_bogus", ["bogus"]);
      assert.fail("expected throw on unknown field");
    } catch (e) {
      const err = e as Error & { code?: string };
      assert.equal(err.code, "SCHEMA_INVALID");
      assert.match(err.message, /not declared on this schema/);
    }
  });

  test("rejects an empty name", () => {
    try {
      schema({ email: t.string() }).index("", ["email"]);
      assert.fail("expected throw on empty name");
    } catch (e) {
      const err = e as Error & { code?: string };
      assert.equal(err.code, "SCHEMA_INVALID");
    }
  });

  test("rejects an empty fields array", () => {
    try {
      schema({ email: t.string() }).index("by_email", []);
      assert.fail("expected throw on empty fields");
    } catch (e) {
      const err = e as Error & { code?: string };
      assert.equal(err.code, "SCHEMA_INVALID");
    }
  });
});

// ---------------------------------------------------------------------------
// Runtime: registerModel wire format carries the indexes
// ---------------------------------------------------------------------------

describe("installSchema — passes named indexes through to native registerModel", () => {
  test("emits an indexes argument with mapped column names per declaration", async () => {
    const calls: { collection: string; schema: ZeroshipDbSchema; indexes: ZeroshipDbNamedIndex[] }[] = [];
    const native = {
      registerModel: (collection: string, sch: ZeroshipDbSchema, indexes?: ZeroshipDbNamedIndex[]) => {
        calls.push({ collection, schema: sch, indexes: indexes ?? [] });
        return Promise.resolve();
      },
      collection() {
        return {
          async find() { return []; },
          async findOne() { return null; },
        };
      },
    } as unknown as ZeroshipDb;

    installSchemaForTest(
      {
        users: schema({
          email: t.string().required(),
          firstName: t.string(),
          done: t.boolean(),
        })
          .index("by_email", ["email"])
          .index("by_first_done", ["firstName", "done"]),
      },
      { native },
    );

    // Drain the microtask the chained registerModel promise rides on.
    await Promise.resolve();
    await Promise.resolve();

    assert.equal(calls.length, 1);
    const got = calls[0];
    assert.equal(got.collection, "users");
    assert.equal(got.indexes.length, 2);
    assert.equal(got.indexes[0].name, "by_email");
    // snakeCase naming strategy is the default — firstName → first_name.
    assert.deepEqual(got.indexes[0].fields, ["email"]);
    assert.deepEqual(got.indexes[1].fields, ["first_name", "done"]);
  });
});

// ---------------------------------------------------------------------------
// Runtime: warning suppression by prefix coverage
// ---------------------------------------------------------------------------

describe("Collection — unindexed-query warning honours declared indexes", () => {
  let warnings: string[] = [];
  let origWarn: typeof console.warn;

  beforeEach(() => {
    warnings = [];
    origWarn = console.warn;
    console.warn = (msg: string) => { warnings.push(String(msg)); };
    (globalThis as { __zeroshipDbWarnIndexInTest?: boolean }).__zeroshipDbWarnIndexInTest = true;
    __zeroshipDbResetIndexWarnings();
  });

  afterEach(() => {
    console.warn = origWarn;
    (globalThis as { __zeroshipDbWarnIndexInTest?: boolean }).__zeroshipDbWarnIndexInTest = false;
    __zeroshipDbResetIndexWarnings();
  });

  function makeDb() {
    const native = {
      registerModel: () => Promise.resolve(),
      collection() {
        return {
          async find() { return []; },
          async findOne() { return null; },
        };
      },
    } as unknown as ZeroshipDb;

    return installSchemaForTest(
      {
        todos: schema({
          userId: t.number(),
          done: t.boolean().default(false),
          title: t.string().required(),
          email: t.string(),
        })
          .index("by_email", ["email"])
          .index("by_user_done", ["userId", "done"]),
      },
      { native },
    );
  }

  test("filter on the single column of `by_email` does NOT warn", async () => {
    const db = makeDb();
    await db.todos.find({ email: "alice@example.com" });
    assert.equal(warnings.length, 0, `unexpected warnings: ${warnings.join(" | ")}`);
  });

  test("filter on the full multi-column index does NOT warn", async () => {
    const db = makeDb();
    await db.todos.find({ userId: 1, done: false });
    assert.equal(warnings.length, 0, `unexpected warnings: ${warnings.join(" | ")}`);
  });

  test("filter on the leading prefix of a multi-column index does NOT warn", async () => {
    // by_user_done = ["userId", "done"]; a `{userId}`-only filter still
    // hits the index because Postgres can use any leftmost prefix.
    const db = makeDb();
    await db.todos.find({ userId: 7 });
    assert.equal(warnings.length, 0, `unexpected warnings: ${warnings.join(" | ")}`);
  });

  test("filter on a non-prefix subset DOES warn and names the declared indexes", async () => {
    // `{done}` is the *second* column of `by_user_done` — Postgres can't
    // use the index without the leading `userId`, so the warning fires.
    const db = makeDb();
    await db.todos.find({ done: true });
    assert.equal(warnings.length, 1, `expected one warning, got: ${warnings.join(" | ")}`);
    assert.match(warnings[0], /unindexed query on "todos"/);
    assert.match(warnings[0], /Declared indexes: by_email, by_user_done/);
  });

  test("filter on a totally unrelated key DOES warn", async () => {
    const db = makeDb();
    await db.todos.find({ title: "buy milk" });
    assert.equal(warnings.length, 1);
    assert.match(warnings[0], /Declared indexes: by_email, by_user_done/);
  });

  test("filter by `id` never warns even without an index declaration", async () => {
    const db = makeDb();
    await db.todos.find({ id: "todo_01" });
    assert.equal(warnings.length, 0);
  });
});
