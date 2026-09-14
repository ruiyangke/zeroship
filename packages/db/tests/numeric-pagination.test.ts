import { test } from "node:test";
import assert from "node:assert/strict";
import { Collection } from "../src/collection.js";
import { t, type IdValue, type PlainObject } from "../src/types.js";
import type { NativeDb } from "../src/native.js";

const fields = { id: t.bigInt().required().primaryKey(), rank: t.bigInt() };
const orders: Record<string, 1 | -1>[] = [{ id: 1 }, { id: -1 }, { rank: 1 }];

for (const id of [0, -7, 7, 9007199254740993n, "7", "記録"] satisfies IdValue[]) {
  for (const orderBy of orders) {
    test(`pagination preserves ${typeof id} identity ${id} with ${JSON.stringify(orderBy)}`, async () => {
      const calls: PlainObject[] = [];
      const rank = 9007199254740995n;
      const native = { collection: () => ({ find: async (filter: PlainObject) => {
        calls.push(filter);
        return calls.length === 1 ? [{ id, rank }, { id, rank }] : [];
      } }) } as unknown as NativeDb;
      const collection = new Collection("records", {
        id: {type: typeof id === "string" ? "string" : "bigInt",required:true,primaryKey:true},
        rank: {type:"bigInt"},
      }, native);
      const first = await collection.find().sort(orderBy).paginate({ numItems: 1 });
      assert.equal(first.error, null);
      assert.equal(first.data!.page[0].id, id);
      assert.equal(first.data!.isDone, false);
      const last = await collection.find().sort(orderBy).paginate({ numItems: 1, cursor: first.data!.continueCursor });
      assert.equal(last.error, null);
      assert.equal(last.data!.isDone, true);
      assert.equal(last.data!.continueCursor, "");
      assert.deepEqual(calls[1], "rank" in orderBy ? {
        $or: [{rank:{$gt:rank}}, {$and:[{rank},{id:{$gt:id}}]}],
      } : { id: { [orderBy.id === 1 ? "$gt" : "$lt"]: id } });
    });
  }
}

test("after accepts zero and bigint identities without changing their representation", async () => {
  const calls: PlainObject[] = [];
  const native = {collection: () => ({find: async (filter: PlainObject) => { calls.push(filter); return []; }})} as unknown as NativeDb;
  const collection = new Collection<typeof fields>("records", {id:{type:"bigInt",required:true,primaryKey:true},rank:{type:"bigInt"}}, native);
  for (const id of [0, 9007199254740993n]) {
    assert.equal((await collection.find().after(id)).error, null);
    assert.deepEqual(calls.at(-1), {id:{$gt:id}});
  }
});

for (const bigint of ["invalid", "1.5", "", null]) {
  test(`pagination rejects malformed bigint cursor ${bigint} before querying`, async () => {
    const native = {collection: () => ({find: async () => assert.fail("invalid cursor reached the database")})} as unknown as NativeDb;
    const collection = new Collection("records", {id:{type:"bigInt",required:true,primaryKey:true}}, native);
    const cursor = btoa(JSON.stringify({orderBy:{id:1}, lastId:{bigint}, lastValues:{id:{bigint}}}));
    const result = await collection.find().paginate({numItems:1,cursor});
    assert.equal((result.error as Error & { code?: string })?.code, "PAGINATE_INVALID_CURSOR");
  });
}
