/**
 * `__makeProcedure` returns a plain callable procedure reference.
 *
 * It intentionally carries no framework hooks. React apps should use
 * TanStack Query directly around the callable:
 *
 *   useQuery({ queryKey: ["todos", input], queryFn: () => list(input) })
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { __makeProcedure, __SERVER_REFERENCE } from "../src/index.js";

describe("__makeProcedure", () => {
  test("the returned thing is callable; invokes call(input)", async () => {
    let received: unknown = null;
    const fn = __makeProcedure<{ x: number }, string>(
      async (input) => {
        received = input;
        return "ok";
      },
      { id: "demo.echo", kind: "query" },
    );

    const out = await fn({ x: 42 });

    assert.equal(out, "ok");
    assert.deepEqual(received, { x: 42 });
  });

  test("attaches id, kind, and default wire metadata", () => {
    const fn = __makeProcedure(async () => null, {
      id: "todos.list",
      kind: "query",
    });

    assert.equal(fn.id, "todos.list");
    assert.equal(fn.kind, "query");
    assert.equal(fn.wire, "json");
  });

  test("preserves explicit wire metadata", () => {
    const fn = __makeProcedure(async () => null, {
      id: "todos.list",
      kind: "query",
      wire: "json",
    });

    assert.equal(fn.wire, "json");
  });

  test("brands the callable as a server reference", () => {
    const fn = __makeProcedure(async () => null, {
      id: "todos.add",
      kind: "mutation",
    });

    assert.equal(fn[__SERVER_REFERENCE as keyof typeof fn], true);
    assert.equal(fn[Symbol.for("zeroship/server-reference") as keyof typeof fn], true);
    assert.equal(Object.keys(fn).includes(String(__SERVER_REFERENCE)), false);
  });

  test("does not attach React Query or cache helper properties", () => {
    const fn = __makeProcedure(async () => null, {
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
      assert.equal(fn[key], undefined, `${key} should not be attached`);
    }
  });
});
