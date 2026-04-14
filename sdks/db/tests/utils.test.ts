import { test, describe } from "node:test";
import assert from "node:assert/strict";
import {
  mapResultDoc,
  mapFilterOutbound,
  translateAggregatePipeline,
} from "../src/utils.js";

describe("mapResultDoc (inbound)", () => {
  test("id → _id", () => {
    const result = mapResultDoc({ id: "abc123", name: "Alice" });
    assert.equal(result.id, "abc123");
  });

  test("created_at → createdAt", () => {
    const now = new Date().toISOString();
    const result = mapResultDoc({ created_at: now });
    assert.equal(result.createdAt, now);
    assert.equal(result.created_at, undefined);
  });

  test("updated_at → updatedAt", () => {
    const now = new Date().toISOString();
    const result = mapResultDoc({ updated_at: now });
    assert.equal(result.updatedAt, now);
    assert.equal(result.updated_at, undefined);
  });

  test("user fields pass through unchanged", () => {
    const result = mapResultDoc({ id: "x", name: "Bob", score: 42 });
    assert.equal(result.name, "Bob");
    assert.equal(result.score, 42);
  });

  test("all three auto-fields together", () => {
    const result = mapResultDoc({
      id: "1",
      created_at: "2024-01-01",
      updated_at: "2024-06-01",
      title: "hello",
    });
    assert.equal(result.id, "1");
    assert.equal(result.createdAt, "2024-01-01");
    assert.equal(result.updatedAt, "2024-06-01");
    assert.equal(result.title, "hello");
  });
});

describe("mapFilterOutbound (outbound)", () => {
  test("_id → id in simple filter", () => {
    const result = mapFilterOutbound({ id: "abc" });
    assert.equal(result.id, "abc");
  });

  test("non-_id fields pass through", () => {
    const result = mapFilterOutbound({ name: "Alice", age: 30 });
    assert.equal(result.name, "Alice");
    assert.equal(result.age, 30);
  });

  test("$and: deep mapping", () => {
    const result = mapFilterOutbound({
      $and: [{ id: "1" }, { name: "Alice" }],
    }) as Record<string, unknown>;
    const and = result.$and as Record<string, unknown>[];
    assert.equal(and[0].id, "1");
    assert.equal(and[1].name, "Alice");
  });

  test("$or: deep mapping", () => {
    const result = mapFilterOutbound({
      $or: [{ id: "1" }, { id: "2" }],
    }) as Record<string, unknown>;
    const or = result.$or as Record<string, unknown>[];
    assert.equal(or[0].id, "1");
    assert.equal(or[1].id, "2");
  });

  test("$not: deep mapping", () => {
    const result = mapFilterOutbound({
      $not: { id: "abc" },
    }) as Record<string, unknown>;
    const not = result.$not as Record<string, unknown>;
    assert.equal(not.id, "abc");
  });

  // I5: createdAt/updatedAt outbound mapping
  test("createdAt → created_at in simple filter", () => {
    const result = mapFilterOutbound({ createdAt: "2024-01-01" });
    assert.equal(result.created_at, "2024-01-01");
    assert.equal(result.createdAt, undefined);
  });

  test("updatedAt → updated_at in simple filter", () => {
    const result = mapFilterOutbound({ updatedAt: "2024-06-01" });
    assert.equal(result.updated_at, "2024-06-01");
    assert.equal(result.updatedAt, undefined);
  });

  test("createdAt/updatedAt deep mapping inside $and", () => {
    const result = mapFilterOutbound({
      $and: [{ createdAt: "2024-01-01" }, { updatedAt: "2024-06-01" }],
    }) as Record<string, unknown>;
    const and = result.$and as Record<string, unknown>[];
    assert.equal(and[0].created_at, "2024-01-01");
    assert.equal(and[0].createdAt, undefined);
    assert.equal(and[1].updated_at, "2024-06-01");
    assert.equal(and[1].updatedAt, undefined);
  });
});

describe("translateAggregatePipeline", () => {
  test("$group.id (string) → $group.by", () => {
    const pipeline = [{ $group: { id: "$category", total: { $sum: 1 } } }];
    const result = translateAggregatePipeline(pipeline);
    const group = result[0].$group as Record<string, unknown>;
    assert.equal(group.by, "category");
  });

  test("{ $sum: 1 } → { $count: true }", () => {
    const pipeline = [{ $group: { id: "$status", count: { $sum: 1 } } }];
    const result = translateAggregatePipeline(pipeline);
    const group = result[0].$group as Record<string, unknown>;
    assert.deepEqual(group.count, { $count: true });
  });

  test("{ $sum: '$field' } → { $sum: 'field' }", () => {
    const pipeline = [
      { $group: { id: "$category", total: { $sum: "$amount" } } },
    ];
    const result = translateAggregatePipeline(pipeline);
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
    const result = translateAggregatePipeline(pipeline);
    const group = result[0].$group as Record<string, unknown>;
    assert.ok(Array.isArray(group.by));
    const by = group.by as string[];
    assert.ok(by.includes("category"));
    assert.ok(by.includes("status"));
  });

  test("$match with _id filter uses mapFilterOutbound", () => {
    const pipeline = [{ $match: { id: "123" } }];
    const result = translateAggregatePipeline(pipeline);
    const match = result[0].$match as Record<string, unknown>;
    assert.equal(match.id, "123");
  });

  test("non-group stages pass through", () => {
    const pipeline = [{ $sort: { createdAt: -1 } }, { $limit: 10 }];
    const result = translateAggregatePipeline(pipeline);
    assert.deepEqual(result[0], { $sort: { createdAt: -1 } });
    assert.deepEqual(result[1], { $limit: 10 });
  });
});
