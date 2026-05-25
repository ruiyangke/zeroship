import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { Query } from "../src/query.js";

type PlainObject = Record<string, unknown>;

function makeMockNative(pages: PlainObject[][]) {
  const calls: { collection: string; filter: PlainObject; opts: PlainObject }[] = [];
  let i = 0;
  const fn = async (
    collection: string,
    filter: PlainObject,
    opts: PlainObject,
  ): Promise<PlainObject[]> => {
    calls.push({ collection, filter, opts });
    return pages[i++] ?? [];
  };
  return { fn, calls };
}

function decode(cursor: string): PlainObject {
  return JSON.parse(Buffer.from(cursor, "base64").toString("utf8"));
}

describe("Query.paginate — validation", () => {
  test("numItems <= 0 rejects with TypeError", async () => {
    const { fn } = makeMockNative([[]]);
    const { data, error } = await new Query("u", {}, fn).paginate({ numItems: 0 });
    assert.equal(data, null);
    assert.ok(error instanceof TypeError);
    assert.match(error.message, /positive integer/);
  });

  test("non-integer numItems rejects", async () => {
    const { fn } = makeMockNative([[]]);
    const { error } = await new Query("u", {}, fn).paginate({ numItems: 1.5 });
    assert.ok(error instanceof TypeError);
  });

  test("invalid (non-base64) cursor rejects", async () => {
    const { fn } = makeMockNative([[]]);
    const { error } = await new Query("u", {}, fn).paginate({
      cursor: "%%%not-base64%%%",
      numItems: 10,
    });
    assert.ok(error !== null);
    assert.match(error.message, /invalid cursor/);
  });

  test("valid base64 but non-JSON cursor rejects", async () => {
    const { fn } = makeMockNative([[]]);
    const garbage = Buffer.from("not json {{").toString("base64");
    const { error } = await new Query("u", {}, fn).paginate({
      cursor: garbage,
      numItems: 10,
    });
    assert.match(error!.message, /invalid cursor/);
  });

  test("valid JSON but wrong shape rejects", async () => {
    const { fn } = makeMockNative([[]]);
    const wrong = Buffer.from(JSON.stringify({ foo: "bar" })).toString("base64");
    const { error } = await new Query("u", {}, fn).paginate({
      cursor: wrong,
      numItems: 10,
    });
    assert.match(error!.message, /invalid cursor/);
  });
});

describe("Query.paginate — first page (id-only sort)", () => {
  test("first page with cursor undefined seeks from start", async () => {
    const rows: PlainObject[] = [
      { id: "1", name: "a" },
      { id: "2", name: "b" },
      { id: "3", name: "c" },
    ];
    const { fn, calls } = makeMockNative([rows]);
    const { data, error } = await new Query("u", {}, fn).paginate({ numItems: 2 });
    assert.equal(error, null);
    assert.ok(data !== null);
    assert.equal(data.page.length, 2);
    assert.deepEqual(data.page[0], { id: "1", name: "a" });
    assert.deepEqual(data.page[1], { id: "2", name: "b" });
    assert.equal(data.isDone, false);
    // No cursor passed → no seek predicate, filter is unchanged.
    assert.deepEqual(calls[0].filter, {});
    // limit = numItems + 1 so we can detect isDone.
    assert.equal(calls[0].opts.limit, 3);
    // Default orderBy is { id: 1 }.
    assert.deepEqual(calls[0].opts.orderBy, { id: 1 });
  });

  test("continueCursor decodes to last page row's id", async () => {
    const rows: PlainObject[] = [
      { id: "1", name: "a" },
      { id: "2", name: "b" },
      { id: "3", name: "c" },
    ];
    const { fn } = makeMockNative([rows]);
    const { data } = await new Query("u", {}, fn).paginate({ numItems: 2 });
    const c = decode(data!.continueCursor);
    assert.deepEqual(c.orderBy, { id: 1 });
    assert.equal(c.lastId, "2");
  });
});

describe("Query.paginate — subsequent page (cursor round-trip)", () => {
  test("subsequent page applies id > lastId seek predicate", async () => {
    // First call returns 3 rows (numItems+1), so kept page = [1,2] and
    // continueCursor.lastId = 2. Second call returns 3 rows again (the
    // +1 row sentinel) → isDone false, kept page = [3,4].
    const first: PlainObject[] = [{ id: "1" }, { id: "2" }, { id: "3" }];
    const second: PlainObject[] = [{ id: "3" }, { id: "4" }, { id: "5" }];
    const { fn, calls } = makeMockNative([first, second]);
    const q = () => new Query("u", {}, fn);

    const r1 = await q().paginate({ numItems: 2 });
    const r2 = await q().paginate({
      cursor: r1.data!.continueCursor,
      numItems: 2,
    });

    assert.deepEqual(calls[1].filter, { id: { $gt: "2" } });
    assert.equal(r2.data!.page.length, 2);
    assert.equal(r2.data!.isDone, false);
  });

  test("OR-merges seek predicate into existing filter via $and", async () => {
    const first: PlainObject[] = [{ id: "1" }, { id: "2" }, { id: "3" }];
    const { fn, calls } = makeMockNative([first, []]);
    const baseFilter = { active: true };

    const r1 = await new Query("u", baseFilter, fn).paginate({ numItems: 2 });
    await new Query("u", baseFilter, fn).paginate({
      cursor: r1.data!.continueCursor,
      numItems: 2,
    });

    assert.deepEqual(calls[1].filter, {
      $and: [{ active: true }, { id: { $gt: "2" } }],
    });
  });
});

describe("Query.paginate — isDone detection", () => {
  test("isDone true when rows < numItems + 1", async () => {
    const { fn } = makeMockNative([[{ id: "1" }, { id: "2" }]]);
    const { data } = await new Query("u", {}, fn).paginate({ numItems: 5 });
    assert.equal(data!.isDone, true);
    assert.equal(data!.page.length, 2);
  });

  test("isDone false when rows = numItems + 1 (pops the extra)", async () => {
    const { fn } = makeMockNative([[{ id: "1" }, { id: "2" }, { id: "3" }]]);
    const { data } = await new Query("u", {}, fn).paginate({ numItems: 2 });
    assert.equal(data!.isDone, false);
    assert.equal(data!.page.length, 2);
  });

  test("empty result is isDone with empty page", async () => {
    const { fn } = makeMockNative([[]]);
    const { data } = await new Query("u", {}, fn).paginate({ numItems: 10 });
    assert.equal(data!.isDone, true);
    assert.deepEqual(data!.page, []);
  });
});

describe("Query.paginate — descending sort", () => {
  test("descending id-only uses $lt seek predicate", async () => {
    const first: PlainObject[] = [{ id: "9" }, { id: "8" }, { id: "7" }];
    const { fn, calls } = makeMockNative([first, []]);

    const r1 = await new Query("u", {}, fn)
      .sort({ id: -1 })
      .paginate({ numItems: 2 });
    assert.deepEqual(calls[0].opts.orderBy, { id: -1 });
    assert.equal(r1.data!.page[0].id, "9");
    assert.equal(r1.data!.page[1].id, "8");
    const c = decode(r1.data!.continueCursor);
    assert.equal(c.lastId, "8");
    assert.deepEqual(c.orderBy, { id: -1 });

    await new Query("u", {}, fn)
      .sort({ id: -1 })
      .paginate({ cursor: r1.data!.continueCursor, numItems: 2 });
    assert.deepEqual(calls[1].filter, { id: { $lt: "8" } });
  });

  test("descending field sort uses $lt + tie-break with id $lt", async () => {
    const first: PlainObject[] = [
      { id: "3", created_at: 1000 },
      { id: "1", created_at: 900 },
      { id: "5", created_at: 800 },
    ];
    const { fn, calls } = makeMockNative([first, []]);
    const r1 = await new Query("u", {}, fn)
      .sort({ created_at: -1 })
      .paginate({ numItems: 2 });
    assert.deepEqual(calls[0].opts.orderBy, { created_at: -1 });
    const c = decode(r1.data!.continueCursor);
    assert.equal(c.lastId, "1");
    assert.equal(c.lastValue, 900);

    await new Query("u", {}, fn)
      .sort({ created_at: -1 })
      .paginate({ cursor: r1.data!.continueCursor, numItems: 2 });
    assert.deepEqual(calls[1].filter, {
      $or: [
        { created_at: { $lt: 900 } },
        { $and: [{ created_at: 900 }, { id: { $lt: "1" } }] },
      ],
    });
  });
});

describe("Query.paginate — non-id sort (ascending)", () => {
  test("ascending name sort builds compound seek with tie-break", async () => {
    const first: PlainObject[] = [
      { id: "7", name: "alice" },
      { id: "3", name: "bob" },
      { id: "9", name: "carol" },
    ];
    const { fn, calls } = makeMockNative([first, []]);

    const r1 = await new Query("u", {}, fn)
      .sort({ name: 1 })
      .paginate({ numItems: 2 });

    assert.deepEqual(calls[0].opts.orderBy, { name: 1 });
    const c = decode(r1.data!.continueCursor);
    assert.equal(c.lastId, "3");
    assert.equal(c.lastValue, "bob");

    await new Query("u", {}, fn)
      .sort({ name: 1 })
      .paginate({ cursor: r1.data!.continueCursor, numItems: 2 });

    assert.deepEqual(calls[1].filter, {
      $or: [
        { name: { $gt: "bob" } },
        { $and: [{ name: "bob" }, { id: { $gt: "3" } }] },
      ],
    });
  });
});

describe("Query.paginate — multi-field sort (compound)", () => {
  test("multi-field orderBy is preserved across pages", async () => {
    const first: PlainObject[] = [
      { id: "1", status: "open", created_at: 100 },
      { id: "2", status: "open", created_at: 200 },
      { id: "3", status: "closed", created_at: 300 },
    ];
    const { fn, calls } = makeMockNative([first, []]);

    const r1 = await new Query("u", {}, fn)
      .sort({ status: 1, created_at: 1 })
      .paginate({ numItems: 2 });

    assert.deepEqual(calls[0].opts.orderBy, { status: 1, created_at: 1 });
    const c = decode(r1.data!.continueCursor);
    assert.deepEqual(c.orderBy, { status: 1, created_at: 1 });
    // The seek's first key is the first orderBy key (status), with id as tie-break.
    assert.equal(c.lastValue, "open");
    assert.equal(c.lastId, "2");

    // Round-trip — same sort accepted.
    const r2 = await new Query("u", {}, fn)
      .sort({ status: 1, created_at: 1 })
      .paginate({ cursor: r1.data!.continueCursor, numItems: 2 });
    assert.equal(r2.error, null);
  });
});

describe("Query.paginate — orderBy mismatch", () => {
  test("cursor from { id: 1 } rejected on { name: 1 } query", async () => {
    const { fn } = makeMockNative([[{ id: "1" }, { id: "2" }, { id: "3" }]]);
    const r1 = await new Query("u", {}, fn).paginate({ numItems: 2 });

    const { error } = await new Query("u", {}, fn)
      .sort({ name: 1 })
      .paginate({ cursor: r1.data!.continueCursor, numItems: 2 });
    assert.match(error!.message, /orderBy mismatch/);
  });

  test("cursor from asc rejected on desc query (direction mismatch)", async () => {
    const { fn } = makeMockNative([[{ id: "1" }, { id: "2" }, { id: "3" }]]);
    const r1 = await new Query("u", {}, fn)
      .sort({ id: 1 })
      .paginate({ numItems: 2 });

    const { error } = await new Query("u", {}, fn)
      .sort({ id: -1 })
      .paginate({ cursor: r1.data!.continueCursor, numItems: 2 });
    assert.match(error!.message, /orderBy mismatch/);
  });
});

describe("Query.paginate — last page (isDone)", () => {
  test("when previous page's continueCursor exhausts the result", async () => {
    const first: PlainObject[] = [{ id: "1" }, { id: "2" }, { id: "3" }];
    const last: PlainObject[] = [{ id: "4" }]; // only 1 row, < numItems + 1 = 3
    const { fn } = makeMockNative([first, last]);

    const r1 = await new Query("u", {}, fn).paginate({ numItems: 2 });
    assert.equal(r1.data!.isDone, false);

    const r2 = await new Query("u", {}, fn).paginate({
      cursor: r1.data!.continueCursor,
      numItems: 2,
    });
    assert.equal(r2.data!.isDone, true);
    assert.equal(r2.data!.page.length, 1);
    assert.equal(r2.data!.page[0].id, "4");
  });
});
