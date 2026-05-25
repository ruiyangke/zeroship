import { test, describe } from "node:test";
import assert from "node:assert/strict";
import {
  mapResultDoc,
  mapFilterOutbound,
  translateAggregatePipeline,
} from "../src/utils.js";
import { naming } from "@zeroship/db";

const { toColumn, toField } = naming.snakeCase;
const SYSTEM_FIELDS = new Set([
  "id",
  "created_at",
  "updated_at",
  "created_by",
  "updated_by",
  "deleted_at",
  "version",
]);
const systemAwareToField = (key: string) => (SYSTEM_FIELDS.has(key) ? key : toField(key));
const systemAwareToColumn = (key: string) => (SYSTEM_FIELDS.has(key) ? key : toColumn(key));

describe("mapResultDoc (inbound)", () => {
  test("id → _id", () => {
    const result = mapResultDoc({ id: "abc123", name: "Alice" }, systemAwareToField);
    assert.equal(result.id, "abc123");
  });

  test("created_at → created_at", () => {
    const now = new Date().toISOString();
    const result = mapResultDoc({ created_at: now }, systemAwareToField);
    assert.equal(result.created_at, now);
    assert.equal((result as any).createdAt, undefined);
  });

  test("updated_at → updated_at", () => {
    const now = new Date().toISOString();
    const result = mapResultDoc({ updated_at: now }, systemAwareToField);
    assert.equal(result.updated_at, now);
    assert.equal((result as any).updatedAt, undefined);
  });

  test("user fields pass through unchanged", () => {
    const result = mapResultDoc({ id: "x", name: "Bob", score: 42 }, systemAwareToField);
    assert.equal(result.name, "Bob");
    assert.equal(result.score, 42);
  });

  test("all three auto-fields together", () => {
    const result = mapResultDoc({
      id: "1",
      created_at: "2024-01-01",
      updated_at: "2024-06-01",
      title: "hello",
    }, systemAwareToField);
    assert.equal(result.id, "1");
    assert.equal(result.created_at, "2024-01-01");
    assert.equal(result.updated_at, "2024-06-01");
    assert.equal(result.title, "hello");
  });
});

describe("mapFilterOutbound (outbound)", () => {
  test("_id → id in simple filter", () => {
    const result = mapFilterOutbound({ id: "abc" }, toColumn);
    assert.equal(result.id, "abc");
  });

  test("non-_id fields pass through", () => {
    const result = mapFilterOutbound({ name: "Alice", age: 30 }, toColumn);
    assert.equal(result.name, "Alice");
    assert.equal(result.age, 30);
  });

  test("$and: deep mapping", () => {
    const result = mapFilterOutbound({
      $and: [{ id: "1" }, { name: "Alice" }],
    }, toColumn) as Record<string, unknown>;
    const and = result.$and as Record<string, unknown>[];
    assert.equal(and[0].id, "1");
    assert.equal(and[1].name, "Alice");
  });

  test("$or: deep mapping", () => {
    const result = mapFilterOutbound({
      $or: [{ id: "1" }, { id: "2" }],
    }, toColumn) as Record<string, unknown>;
    const or = result.$or as Record<string, unknown>[];
    assert.equal(or[0].id, "1");
    assert.equal(or[1].id, "2");
  });

  test("$not: deep mapping", () => {
    const result = mapFilterOutbound({
      $not: { id: "abc" },
    }, toColumn) as Record<string, unknown>;
    const not = result.$not as Record<string, unknown>;
    assert.equal(not.id, "abc");
  });

  test("created_at → created_at in simple filter", () => {
    const result = mapFilterOutbound({ created_at: "2024-01-01" }, systemAwareToColumn);
    assert.equal(result.created_at, "2024-01-01");
    assert.equal((result as any).createdAt, undefined);
  });

  test("updated_at → updated_at in simple filter", () => {
    const result = mapFilterOutbound({ updated_at: "2024-06-01" }, systemAwareToColumn);
    assert.equal(result.updated_at, "2024-06-01");
    assert.equal((result as any).updatedAt, undefined);
  });

  test("created_at/updated_at deep mapping inside $and", () => {
    const result = mapFilterOutbound({
      $and: [{ created_at: "2024-01-01" }, { updated_at: "2024-06-01" }],
    }, systemAwareToColumn) as Record<string, unknown>;
    const and = result.$and as Record<string, unknown>[];
    assert.equal(and[0].created_at, "2024-01-01");
    assert.equal((and[0] as any).createdAt, undefined);
    assert.equal(and[1].updated_at, "2024-06-01");
    assert.equal((and[1] as any).updatedAt, undefined);
  });
});

describe("translateAggregatePipeline", () => {
  test("$group.id (string) → $group.by", () => {
    const pipeline = [{ $group: { id: "$category", total: { $sum: 1 } } }];
    const result = translateAggregatePipeline(pipeline, toColumn);
    const group = result[0].$group as Record<string, unknown>;
    assert.equal(group.by, "category");
  });

  test("{ $sum: 1 } → { $count: true }", () => {
    const pipeline = [{ $group: { id: "$status", count: { $sum: 1 } } }];
    const result = translateAggregatePipeline(pipeline, toColumn);
    const group = result[0].$group as Record<string, unknown>;
    assert.deepEqual(group.count, { $count: true });
  });

  test("{ $sum: '$field' } → { $sum: 'field' }", () => {
    const pipeline = [
      { $group: { id: "$category", total: { $sum: "$amount" } } },
    ];
    const result = translateAggregatePipeline(pipeline, toColumn);
    const group = result[0].$group as Record<string, unknown>;
    assert.deepEqual(group.total, { $sum: "amount" });
  });

  test("multi-field group _id → by array", () => {
    const pipeline = [
      {
        $group: {
          id: { category: "$category", status: "$status" },
          count: { $sum: 1 },
        },
      },
    ];
    const result = translateAggregatePipeline(pipeline, toColumn);
    const group = result[0].$group as Record<string, unknown>;
    assert.ok(Array.isArray(group.by));
    const by = group.by as string[];
    assert.ok(by.includes("category"));
    assert.ok(by.includes("status"));
  });

  test("$match with _id filter uses mapFilterOutbound", () => {
    const pipeline = [{ $match: { id: "123" } }];
    const result = translateAggregatePipeline(pipeline, toColumn);
    const match = result[0].$match as Record<string, unknown>;
    assert.equal(match.id, "123");
  });

  test("non-group stages map field names", () => {
    const pipeline = [{ $sort: { created_at: -1 } }, { $limit: 10 }];
    const result = translateAggregatePipeline(pipeline, toColumn);
    assert.deepEqual(result[0], { $sort: { created_at: -1 } });
    assert.deepEqual(result[1], { $limit: 10 });
  });

  test("$having maps field names", () => {
    const pipeline = [{ $having: { created_at: { $gt: 100 } } }];
    const result = translateAggregatePipeline(pipeline, systemAwareToColumn);
    const having = result[0].$having as Record<string, unknown>;
    assert.ok("created_at" in having);
    assert.deepEqual(having.created_at, { $gt: 100 });
  });

  test("$match preserves snake_case system fields", () => {
    const pipeline = [{ $match: { firstName: "Alice", updated_at: { $gt: 100 } } }];
    const result = translateAggregatePipeline(pipeline, systemAwareToColumn);
    const match = result[0].$match as Record<string, unknown>;
    assert.equal(match.first_name, "Alice");
    assert.ok("updated_at" in match);
    assert.equal(match.firstName, undefined);
    assert.deepEqual(match.updated_at, { $gt: 100 });
  });
});

// ---------------------------------------------------------------------------
// mapResultDoc — general strategy
// ---------------------------------------------------------------------------

describe("mapResultDoc with custom strategy", () => {
  test("asIs toField passes keys unchanged", () => {
    const doc = { first_name: "Alice", id: 1 };
    const result = mapResultDoc(doc, naming.asIs.toField);
    assert.equal(result.first_name, "Alice");
    assert.equal(result.firstName, undefined);
  });

  test("snakeCase toField converts all snake_case keys", () => {
    const doc = { first_name: "Alice", last_name: "Smith", id: 1, email: "a@b.com" };
    const result = mapResultDoc(doc, toField);
    assert.equal(result.firstName, "Alice");
    assert.equal(result.lastName, "Smith");
    assert.equal(result.id, 1);
    assert.equal(result.email, "a@b.com");
    assert.equal(result.first_name, undefined);
    assert.equal(result.last_name, undefined);
  });
});

// ---------------------------------------------------------------------------
// mapFilterOutbound — general strategy
// ---------------------------------------------------------------------------

describe("mapFilterOutbound with naming strategy", () => {
  test("snakeCase converts camelCase field names", () => {
    const result = mapFilterOutbound({ firstName: "Alice", age: 30 }, toColumn);
    assert.equal(result.first_name, "Alice");
    assert.equal(result.age, 30);
    assert.equal(result.firstName, undefined);
  });

  test("fast path returns original for all-lowercase filter", () => {
    const filter = { id: 1, name: "Alice" };
    const result = mapFilterOutbound(filter, toColumn);
    assert.equal(result, filter); // same reference — no copy
  });

  test("$and recursion maps nested camelCase keys", () => {
    const result = mapFilterOutbound({
      $and: [{ firstName: "Alice" }, { lastName: "Smith" }],
    }, toColumn) as Record<string, unknown>;
    const and = result.$and as Record<string, unknown>[];
    assert.equal(and[0].first_name, "Alice");
    assert.equal(and[1].last_name, "Smith");
  });

  test("depth limit throws on deeply nested filters", () => {
    let filter: Record<string, unknown> = { id: 1 };
    for (let i = 0; i < 25; i++) {
      filter = { $not: filter };
    }
    assert.throws(() => mapFilterOutbound(filter, toColumn), /too deep/);
  });

  test("asIs toColumn passes all keys unchanged", () => {
    const filter = { firstName: "Alice", created_at: 100 };
    const result = mapFilterOutbound(filter, naming.asIs.toColumn);
    assert.equal(result, filter); // same reference
  });
});
