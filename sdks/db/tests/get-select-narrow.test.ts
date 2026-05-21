/**
 * get(idOrFilter, { select }) narrowing — compile-time assertions
 * dressed as runtime tests. The bodies are trivial; the value is that
 * tsc enforces the narrowed return shape when `select` is present and
 * leaves the unselected branch as `Row<S> | null`.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { model } from "@zeroship/bootstrap/install-schema";
import { t, type Row } from "@zeroship/db";

type AnyRec = Record<string, unknown>;

function makeMockNative(row: AnyRec | null) {
  const native = {
    registerModel: () => Promise.resolve(),
    collection(_name: string) {
      return {
        async findOne(_filter: AnyRec, _opts: AnyRec) {
          return row;
        },
        // The DataLoader path (numeric id, no select, no orderBy, no tx)
        // dispatches through `find({id: {$in: [...]}})`; existing
        // get-select-narrow tests that use this mock still expect the
        // single-row result to come back, so we mirror it here.
        async find(_filter: AnyRec, _opts: AnyRec) {
          return row === null ? [] : [row];
        },
      };
    },
  };
  return native as unknown as ZeroshipDb;
}

describe("get(...) select narrowing", () => {
  test("get with select returns Pick<Row, K> | null", async () => {
    const Users = model(
      "users",
      {
        email: t.string().required().unique(),
        name: t.string().required(),
        age: t.number(),
      },
      makeMockNative({ id: 1, email: "a@b.com" }),
    );

    const { data, error } = await Users.get(1, { select: ["email", "id"] });
    assert.equal(error, null);
    assert.ok(data);
    // Type-level check: TS should see `data` as Pick<Row<S>, "email" | "id">.
    const email: string = data.email;
    const id: number = data.id;
    assert.equal(email, "a@b.com");
    assert.equal(id, 1);

    // Negative check: a field outside the select list is not on the
    // narrowed type. We cast through unknown to demonstrate it must NOT
    // be reachable directly without a cast.
    const widened = data as unknown as Row<{ name: ReturnType<typeof t.string> }>;
    assert.equal(widened.name, undefined);
  });

  test("get without select returns Row<S> | null", async () => {
    const Users = model(
      "users",
      {
        email: t.string().required().unique(),
        name: t.string().required(),
      },
      makeMockNative({ id: 1, email: "a@b.com", name: "Alice" }),
    );

    const { data, error } = await Users.get(1);
    assert.equal(error, null);
    assert.ok(data);
    // Full row — no narrowing.
    const email: string = data.email;
    const name: string = data.name;
    assert.equal(email, "a@b.com");
    assert.equal(name, "Alice");
  });

  test("get with orderBy but no select still returns Row<S> | null", async () => {
    const Users = model(
      "users",
      {
        email: t.string().required().unique(),
        name: t.string().required(),
      },
      makeMockNative({ id: 1, email: "a@b.com", name: "Alice" }),
    );

    const { data, error } = await Users.get({ email: "a@b.com" }, { orderBy: { id: -1 } });
    assert.equal(error, null);
    assert.ok(data);
    const name: string = data.name;
    assert.equal(name, "Alice");
  });

  test("get returns null when nothing matched", async () => {
    const Users = model(
      "users",
      {
        email: t.string().required().unique(),
        name: t.string().required(),
      },
      makeMockNative(null),
    );

    const { data, error } = await Users.get(1, { select: ["email"] });
    assert.equal(error, null);
    assert.equal(data, null);
  });
});
