import assert from "node:assert/strict";
import test from "node:test";
import { loadRelations } from "../src/collection/relations.js";

test("eager relations share a transaction connection without overlapping queries", async () => {
  let busy = false;
  const target = {
    async find() {
      assert.equal(busy, false, "the transaction connection is already in use");
      busy = true;
      await new Promise<void>((resolve) => setImmediate(resolve));
      busy = false;
      return { data: [{ id: "person_a", name: "Ada" }], error: null };
    },
  };
  const collection = {
    _txDepth: 1,
    _name: "posts",
    _schema: {
      author: { type: "string" as const, refTarget: "people" },
      reviewer: { type: "string" as const, refTarget: "people" },
    },
    _resolveCollection: () => target,
  };
  const rows = [{ author: "person_a", reviewer: "person_a" }];
  await loadRelations(collection, rows, { author: true, reviewer: true });
  assert.deepEqual(rows, [{
    author: { id: "person_a", name: "Ada" },
    reviewer: { id: "person_a", name: "Ada" },
  }]);
});
