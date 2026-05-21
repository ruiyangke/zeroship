/**
 * R7 m3 + m4 regression — `Query.paginate` cursor semantics.
 *
 *  m3. `paginate` truncated bigint ids via `Number(...)`. The next page's
 *      seek predicate would then point at a rounded id and either skip
 *      rows or repeat them. Closed with the same `BigInt(Number(v)) === v`
 *      round-trip used by `_loadRelations` in `collection.ts`, returning
 *      `paginate_cursor_precision_loss` on overflow.
 *
 *  m4. `paginate` returned the caller's input cursor when `isDone === true`.
 *      Callers keying on `continueCursor === ""` to detect terminal state
 *      were misled (the `isDone` flag was the correct sentinel, but the
 *      cursor field was misleading). Closed by always returning `""`
 *      when the page is terminal.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { Query } from "../src/query.js";

type PlainObject = Record<string, unknown>;

function makeMockNative(pages: PlainObject[][]) {
  let i = 0;
  const fn = async (
    _collection: string,
    _filter: PlainObject,
    _opts: PlainObject,
  ): Promise<PlainObject[]> => {
    return pages[i++] ?? [];
  };
  return { fn };
}

describe("R7 m3 — Query.paginate bigint-precision check on cursor id", () => {
  test("bigint id > MAX_SAFE_INTEGER on last kept row rejects with code", async () => {
    const huge = (1n << 60n) + 7n; // way above MAX_SAFE_INTEGER
    const rows: PlainObject[] = [
      { id: 1, name: "a" },
      { id: huge, name: "huge" },
      { id: 99, name: "tail" }, // sentinel +1 row so isDone=false
    ];
    const { fn } = makeMockNative([rows]);
    const { data, error } = await new Query("u", {}, fn).paginate({ numItems: 2 });
    assert.equal(data, null);
    assert.ok(error !== null);
    const err = error as Error & { code?: string };
    assert.equal(err.code, "paginate_cursor_precision_loss");
    assert.match(err.message, /precision/);
  });

  test("bigint id within MAX_SAFE_INTEGER round-trips losslessly", async () => {
    const safe = 9007199254740991n; // exactly 2^53 - 1
    const rows: PlainObject[] = [
      { id: 1 },
      { id: safe },
      { id: 3 }, // sentinel +1
    ];
    const { fn } = makeMockNative([rows]);
    const { data, error } = await new Query("u", {}, fn).paginate({ numItems: 2 });
    assert.equal(error, null);
    assert.ok(data !== null);
    // Cursor is non-empty (page is not done) and decodes to the safe-bigint id.
    assert.notEqual(data.continueCursor, "");
    const decoded = JSON.parse(Buffer.from(data.continueCursor, "base64").toString("utf8"));
    assert.equal(decoded.lastId, Number(safe));
  });

  test("non-number, non-bigint id rejects with paginate_invalid_id", async () => {
    const rows: PlainObject[] = [
      { id: 1 },
      { id: "abc" }, // garbage
      { id: 3 },
    ];
    const { fn } = makeMockNative([rows]);
    const { error } = await new Query("u", {}, fn).paginate({ numItems: 2 });
    assert.ok(error !== null);
    const err = error as Error & { code?: string };
    assert.equal(err.code, "paginate_invalid_id");
  });
});

describe("R7 m4 — Query.paginate returns continueCursor === \"\" when isDone", () => {
  test("first-and-only page (rows < numItems+1): continueCursor is \"\"", async () => {
    const { fn } = makeMockNative([[{ id: 1 }, { id: 2 }]]);
    const { data } = await new Query("u", {}, fn).paginate({ numItems: 5 });
    assert.equal(data!.isDone, true);
    assert.equal(data!.continueCursor, "");
  });

  test("empty result: continueCursor is \"\"", async () => {
    const { fn } = makeMockNative([[]]);
    const { data } = await new Query("u", {}, fn).paginate({ numItems: 10 });
    assert.equal(data!.isDone, true);
    assert.equal(data!.continueCursor, "");
  });

  test("subsequent terminal page does NOT carry the input cursor through", async () => {
    // First page hands out a non-empty cursor; second page is the last
    // page (rows < numItems+1) and must return "" — not the input cursor.
    const first: PlainObject[] = [{ id: 1 }, { id: 2 }, { id: 3 }];
    const last: PlainObject[] = [{ id: 4 }]; // 1 row, < numItems+1 = 3
    const { fn } = makeMockNative([first, last]);

    const r1 = await new Query("u", {}, fn).paginate({ numItems: 2 });
    assert.equal(r1.data!.isDone, false);
    assert.notEqual(r1.data!.continueCursor, "");

    const r2 = await new Query("u", {}, fn).paginate({
      cursor: r1.data!.continueCursor,
      numItems: 2,
    });
    assert.equal(r2.data!.isDone, true);
    // Previously this returned r1.data!.continueCursor verbatim — now "".
    assert.equal(r2.data!.continueCursor, "");
  });

  test("non-terminal subsequent page still produces a non-empty cursor", async () => {
    const first: PlainObject[] = [{ id: 1 }, { id: 2 }, { id: 3 }];
    const second: PlainObject[] = [{ id: 3 }, { id: 4 }, { id: 5 }];
    const { fn } = makeMockNative([first, second]);

    const r1 = await new Query("u", {}, fn).paginate({ numItems: 2 });
    const r2 = await new Query("u", {}, fn).paginate({
      cursor: r1.data!.continueCursor,
      numItems: 2,
    });
    assert.equal(r2.data!.isDone, false);
    assert.notEqual(r2.data!.continueCursor, "");
    // And it must be a fresh cursor advancing past id=4, not the r1 cursor.
    assert.notEqual(r2.data!.continueCursor, r1.data!.continueCursor);
  });
});
