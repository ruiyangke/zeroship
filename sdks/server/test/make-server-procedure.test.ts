/**
 * `__makeServerProcedure` returns a plain callable procedure reference
 * with metadata matching the client stub surface.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { __makeServerProcedure } from "../src/index.js";

describe("__makeServerProcedure", () => {
  test("returned object is callable", async () => {
    const impl = async (input: { limit: number }): Promise<number[]> =>
      [1, 2, 3].slice(0, input.limit);
    const list = __makeServerProcedure(impl, {
      id: "todos.list",
      kind: "query",
    });

    const result = await list({ limit: 2 });

    assert.deepEqual(result, [1, 2]);
  });

  test("attaches id, kind, and default wire metadata", () => {
    const list = __makeServerProcedure(async () => [], {
      id: "todos.list",
      kind: "query",
    });

    assert.equal(list.id, "todos.list");
    assert.equal(list.kind, "query");
    assert.equal(list.wire, "json");
  });

  test("preserves explicit wire metadata", () => {
    const list = __makeServerProcedure(async () => [], {
      id: "todos.list",
      kind: "query",
      wire: "json",
    });

    assert.equal(list.wire, "json");
  });

  test("does not attach framework hook helpers", () => {
    const list = __makeServerProcedure(async () => [], {
      id: "todos.list",
      kind: "query",
    }) as Record<string, unknown>;

    for (const key of [
      "queryKey",
      "useQuery",
      "useSuspenseQuery",
      "useInfiniteQuery",
      "useMutation",
      "useStream",
      "useSubscription",
      "invalidate",
      "prefetch",
      "setData",
    ]) {
      assert.equal(list[key], undefined, `${key} should not be attached`);
    }
  });
});
