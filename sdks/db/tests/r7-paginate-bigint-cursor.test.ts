/**
 * R7 m4 regression — `Query.paginate` cursor semantics.
 *
 * The cursor now carries string ids directly. Terminal pages still
 * normalize `continueCursor` to `""`.
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

describe("Query.paginate string-id cursor handling", () => {
  test("string id round-trips through continueCursor", async () => {
    const rows: PlainObject[] = [
      { id: "row_1" },
      { id: "row_2" },
      { id: "row_3" }, // sentinel +1
    ];
    const { fn } = makeMockNative([rows]);
    const { data, error } = await new Query("u", {}, fn).paginate({ numItems: 2 });
    assert.equal(error, null);
    assert.ok(data !== null);
    assert.notEqual(data.continueCursor, "");
    const decoded = JSON.parse(Buffer.from(data.continueCursor, "base64").toString("utf8"));
    assert.equal(decoded.lastId, "row_2");
  });

  test("non-string id rejects with paginate_invalid_id", async () => {
    const rows: PlainObject[] = [
      { id: "row_1" },
      { id: 2 },
      { id: "row_3" },
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
    const { fn } = makeMockNative([[{ id: "1" }, { id: "2" }]]);
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
    const first: PlainObject[] = [{ id: "1" }, { id: "2" }, { id: "3" }];
    const last: PlainObject[] = [{ id: "4" }]; // 1 row, < numItems+1 = 3
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
    const first: PlainObject[] = [{ id: "1" }, { id: "2" }, { id: "3" }];
    const second: PlainObject[] = [{ id: "3" }, { id: "4" }, { id: "5" }];
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
