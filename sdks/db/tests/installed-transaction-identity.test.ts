import { test } from "node:test";
import assert from "node:assert/strict";
import { t } from "@zeroship/db";
import { installSchema, type Db } from "@zeroship/bootstrap/install-schema";
import type { NativeDb } from "../src/native.js";

const schema = {records:{id:t.bigInt().required().primaryKey(),label:t.string()}};

test("installed transaction wrappers preserve numeric identity types and values", async () => {
  const id = 9007199254740993n;
  const calls: unknown[] = [];
  const mutate = async (filter: unknown) => { calls.push(filter); return {id,label:"value"}; };
  const native = {
    transaction: async (body: (raw: unknown) => unknown) => body(undefined),
    collection: () => ({
      find: async (filter: unknown) => [await mutate(filter)],
      update: mutate, delete: mutate, purge: mutate, restore: mutate,
      bulkUnmask: async () => ({results:{[String(id)]:{label:"private"}}}),
    }),
  } as unknown as NativeDb;
  installSchema(schema, native, {descriptor:{version:2,collections:{records:{
    fields:{id:{type:"bigInt",required:true,primaryKey:true},label:{type:"string"}},
    options:{softDelete:false,versioning:false},indexes:[],
  }}}});
  const db = native as unknown as Db<typeof schema>;
  const result = await db.transaction(async tx => {
    assert.equal((await tx.records.get(id))?.id, id);
    await tx.records.update(id,{label:"changed"});
    await tx.records.delete(id);
    await tx.records.purge(id);
    await tx.records.restore(id);
    const unmasked = await tx.records.bulkUnmask([{id,columns:["label"]}],{actor:{kind:"support",id:"usr_reader"}});
    assert.deepEqual(unmasked.get(id), {label:"private"});
    await tx.records.find().after(id);
    return id;
  });
  assert.equal(result.error, null);
  assert.equal(result.data, id);
  assert.deepEqual(calls, [{id},{id},{id},{id},{id},{id:{$gt:id}}]);
});

function contracts(db: Db<typeof schema>) {
  void db.transaction(async tx => {
    // @ts-expect-error Installed transactions reject text identities for numeric schemas.
    await tx.records.get("7");
    // @ts-expect-error Installed transaction cursors follow the same schema.
    await tx.records.find().after("7");
  });
}
void contracts;
