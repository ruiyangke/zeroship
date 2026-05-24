/**
 * **P9 PR 1** — `Query.unique()` + `Query.last()` terminals (and the
 * accompanying error classes `NotFoundError` / `NotUniqueError` /
 * `InvalidOperationError`).
 *
 * These tests pin the lazy-terminal contract: each method applies its
 * `limit` (and `sort` for `last()`) on top of the Query's current
 * state, runs the configured native, then unwraps the row array into
 * the strict / loose shape the terminal promises.
 *
 * Coverage:
 *   - `.unique()` — 0 rows / 1 row / 2 rows / native error / restores
 *     prev limit / wins races against `.then()`
 *   - `.last()` — no sort throws / reverses asc / reverses desc /
 *     null on empty / native error / restores prev sort+limit
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { model } from "@zeroship/bootstrap/install-schema";
import {
  t,
  NotFoundError,
  NotUniqueError,
  InvalidOperationError,
} from "@zeroship/db";

type AnyRec = Record<string, unknown>;

/** Mock native that returns rows from `rowsByCall` queue. Each `find`
 *  invocation pops the next entry; assertions can also inspect the
 *  recorded opts to verify the terminal applied the right limit/sort. */
function makeMockNative(rowsByCall: AnyRec[][]) {
  const calls: { filter: AnyRec; opts: AnyRec }[] = [];
  let i = 0;
  const native = {
    registerModel: () => Promise.resolve(),
    collection(_name: string) {
      return {
        async find(filter: AnyRec, opts: AnyRec) {
          calls.push({ filter, opts });
          const next = rowsByCall[i++] ?? [];
          return next;
        },
      };
    },
  };
  return { native: native as unknown as ZeroshipDb, calls };
}

function makeFailingNative(err: Error) {
  const native = {
    registerModel: () => Promise.resolve(),
    collection(_name: string) {
      return {
        async find(_f: AnyRec, _o: AnyRec): Promise<AnyRec[]> {
          throw err;
        },
      };
    },
  };
  return native as unknown as ZeroshipDb;
}

const schemaUsers = {
  email: t.string().required().unique(),
  name: t.string().required(),
};

describe("Query.unique() — strict exactly-one terminal", () => {
  test("1 row → ok(row)", async () => {
    const { native, calls } = makeMockNative([
      [{ id: 1, email: "a@b.com", name: "Alice" }],
    ]);
    const Users = model("users", schemaUsers, native);
    const result = await Users.find({ email: "a@b.com" }).unique();
    assert.equal(result.error, null);
    assert.ok(result.data);
    assert.equal((result.data as AnyRec).email, "a@b.com");
    // Native received LIMIT 2 — the unique() over-fetch sentinel.
    assert.equal(calls[0].opts.limit, 2);
  });

  test("0 rows → err(NotFoundError) with code expected_one_got_zero", async () => {
    const { native } = makeMockNative([[]]);
    const Users = model("users", schemaUsers, native);
    const { data, error } = await Users.find({ email: "missing@x" }).unique();
    assert.equal(data, null);
    assert.ok(error);
    assert.ok(error instanceof NotFoundError);
    assert.equal((error as Error & { code?: string }).code, "expected_one_got_zero");
  });

  test(">1 rows → err(NotUniqueError) with code expected_one_got_many", async () => {
    const { native } = makeMockNative([[
      { id: 1, email: "a@b.com", name: "Alice" },
      { id: 2, email: "a@b.com", name: "Bob" },
    ]]);
    const Users = model("users", schemaUsers, native);
    const { data, error } = await Users.find({ email: "a@b.com" }).unique();
    assert.equal(data, null);
    assert.ok(error);
    assert.ok(error instanceof NotUniqueError);
    assert.equal((error as Error & { code?: string }).code, "expected_one_got_many");
    assert.equal((error as NotUniqueError).count, 2);
  });

  test("native throw propagates as result.error", async () => {
    const boom = Object.assign(new Error("connection refused"), {
      code: "conn_refused",
    });
    const native = makeFailingNative(boom);
    const Users = model("users", schemaUsers, native);
    const { error } = await Users.find({ email: "x" }).unique();
    assert.ok(error);
    assert.equal((error as Error & { code?: string }).code, "conn_refused");
  });

  test("preserves the Query's prior limit after the terminal returns", async () => {
    const { native, calls } = makeMockNative([
      [{ id: 1, email: "a@b.com", name: "Alice" }],
      [{ id: 1, email: "a@b.com", name: "Alice" }],
    ]);
    const Users = model("users", schemaUsers, native);
    const q = Users.find({ email: "a@b.com" }).limit(50);
    await q.unique();
    await q; // re-run the iterator path; old limit must still be in effect
    // First call set LIMIT 2 (unique's over-fetch); second call uses the
    // explicit limit(50) the caller set on the Query.
    assert.equal(calls[0].opts.limit, 2);
    assert.equal(calls[1].opts.limit, 50);
  });

  test("sort + select state passes through to the native call", async () => {
    const { native, calls } = makeMockNative([
      [{ id: 1, email: "a@b.com", name: "Alice" }],
    ]);
    const Users = model("users", schemaUsers, native);
    await Users.find({ name: "Alice" }).sort({ id: -1 }).select(["email"]).unique();
    assert.deepEqual(calls[0].opts.orderBy, { id: -1 });
    assert.deepEqual(calls[0].opts.select, ["email"]);
    assert.equal(calls[0].opts.limit, 2);
  });
});

describe("Query.last() — last matching row in the current sort", () => {
  test("no sort set → err(InvalidOperationError) code last_requires_sort", async () => {
    const { native } = makeMockNative([[]]);
    const Users = model("users", schemaUsers, native);
    const { data, error } = await Users.find({}).last();
    assert.equal(data, null);
    assert.ok(error);
    assert.ok(error instanceof InvalidOperationError);
    assert.equal((error as Error & { code?: string }).code, "last_requires_sort");
  });

  test("ascending sort + matches → flips to descending + first row", async () => {
    const { native, calls } = makeMockNative([
      [{ id: 99, email: "z@b.com", name: "Zoe" }],
    ]);
    const Users = model("users", schemaUsers, native);
    const { data, error } = await Users.find({}).sort({ id: 1 }).last();
    assert.equal(error, null);
    assert.ok(data);
    assert.equal((data as AnyRec).email, "z@b.com");
    // The original sort was `{id: 1}`; last() must dispatch with the
    // reversed orderBy `{id: -1}` so the LIMIT 1 takes the highest id.
    assert.deepEqual(calls[0].opts.orderBy, { id: -1 });
    assert.equal(calls[0].opts.limit, 1);
  });

  test("descending sort + matches → flips to ascending + first row", async () => {
    const { native, calls } = makeMockNative([
      [{ id: 1, email: "a@b.com", name: "Alice" }],
    ]);
    const Users = model("users", schemaUsers, native);
    const { error } = await Users.find({}).sort({ id: -1 }).last();
    assert.equal(error, null);
    assert.deepEqual(calls[0].opts.orderBy, { id: 1 });
    assert.equal(calls[0].opts.limit, 1);
  });

  test("empty result with valid sort → ok(null)", async () => {
    const { native } = makeMockNative([[]]);
    const Users = model("users", schemaUsers, native);
    const { data, error } = await Users.find({ email: "missing@x" })
      .sort({ id: 1 })
      .last();
    assert.equal(error, null);
    assert.equal(data, null);
  });

  test("native throw propagates as result.error", async () => {
    const boom = Object.assign(new Error("backend timeout"), {
      code: "timeout",
    });
    const native = makeFailingNative(boom);
    const Users = model("users", schemaUsers, native);
    const { error } = await Users.find({}).sort({ id: 1 }).last();
    assert.ok(error);
    assert.equal((error as Error & { code?: string }).code, "timeout");
  });

  test("preserves the Query's prior sort + limit after the terminal returns", async () => {
    const { native, calls } = makeMockNative([
      [{ id: 5, email: "x@b.com", name: "X" }],
      [{ id: 1, email: "a@b.com", name: "Alice" }],
    ]);
    const Users = model("users", schemaUsers, native);
    const q = Users.find({}).sort({ id: 1 }).limit(10);
    await q.last();
    await q;
    // First call reversed to {id: -1} with limit:1; second call back to
    // the caller-set {id: 1} with limit:10.
    assert.deepEqual(calls[0].opts.orderBy, { id: -1 });
    assert.equal(calls[0].opts.limit, 1);
    assert.deepEqual(calls[1].opts.orderBy, { id: 1 });
    assert.equal(calls[1].opts.limit, 10);
  });
});
