/**
 * Robustness — AND/OR composition semantics (Gap DD).
 *
 * Mongo's filter language: a flat object with mixed field equalities and
 * top-level `$or` composes as
 *
 *   { email: "x", $or: [{ active: true }, { active: false }] }
 *
 * → WHERE email = 'x' AND (active = true OR active = false)
 *
 * In other words, sibling keys at the same level are joined with AND;
 * `$or` only groups its array elements. This file pins the dispatched
 * filter shape so the contract is explicit — a future AI builder that
 * reads this test learns the semantics without guessing.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { model } from "../src/model.js";
import { t } from "../src/types.js";
import { naming } from "../src/types.js";

type AnyRec = Record<string, unknown>;

function makeMockNative() {
  const calls: { method: string; collection: string; filter: AnyRec; opts: AnyRec }[] = [];
  const native = {
    registerModel: () => Promise.resolve(),
    collection(name: string) {
      return {
        async find(filter: AnyRec, opts: AnyRec) {
          calls.push({ method: "find", collection: name, filter, opts });
          return [];
        },
      };
    },
  };
  return { native: native as unknown as ZeroshipDb, calls };
}

describe("Filter AND/OR semantics (Mongo-compatible)", () => {
  test("sibling keys + $or compose as AND( field=x , OR(...) )", async () => {
    const { native, calls } = makeMockNative();
    const Users = model(
      "users",
      {
        email: t.string().required().unique(),
        active: t.boolean(),
      },
      native,
      // asIs naming so the column key in the dispatched filter equals the
      // schema field — easier to assert without snake/camel conversion.
      naming.asIs,
    );
    await Users.find({
      email: "x",
      $or: [{ active: true }, { active: false }],
    });
    assert.equal(calls.length, 1);
    const dispatched = calls[0].filter;
    // Both keys preserved at the top level (the SDK does NOT collapse
    // `email` into the $or — that would change the semantics).
    assert.equal(dispatched.email, "x");
    assert.ok(Array.isArray(dispatched.$or));
    const orArr = dispatched.$or as AnyRec[];
    assert.equal(orArr.length, 2);
    assert.deepEqual(orArr[0], { active: true });
    assert.deepEqual(orArr[1], { active: false });
  });

  test("nested $and inside $or preserves grouping", async () => {
    const { native, calls } = makeMockNative();
    const Users = model(
      "users",
      {
        name: t.string().required(),
        age: t.number(),
        role: t.string(),
      },
      native,
      naming.asIs,
    );
    await Users.find({
      $or: [
        { $and: [{ age: { $gte: 18 } }, { role: "admin" }] },
        { role: "owner" },
      ],
    });
    const dispatched = calls[0].filter;
    assert.ok(Array.isArray(dispatched.$or));
    const orArr = dispatched.$or as AnyRec[];
    const firstBranch = orArr[0] as { $and?: AnyRec[] };
    assert.ok(Array.isArray(firstBranch.$and));
    assert.equal(firstBranch.$and!.length, 2);
  });

  test("bare $or with no sibling keys passes through unchanged", async () => {
    const { native, calls } = makeMockNative();
    const Users = model(
      "users",
      { active: t.boolean() },
      native,
      naming.asIs,
    );
    await Users.find({ $or: [{ active: true }, { active: false }] });
    const dispatched = calls[0].filter;
    assert.deepEqual(Object.keys(dispatched), ["$or"]);
  });
});
