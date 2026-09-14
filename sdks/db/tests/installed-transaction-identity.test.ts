import { test } from "node:test";
import assert from "node:assert/strict";
import { t, type Db } from "../src/index.js";
import { installSchema } from "../../../crates/zeroship-data-v8/js/testing.js";
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
  installSchema(native, {version:2,collections:{records:{
    fields:{id:{type:"bigInt",required:true,primaryKey:true},label:{type:"string"}},
    options:{softDelete:false,versioning:false},indexes:[],
  }}});
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
    const records = tx.collection("records");
    await records.get(1n);
    // @ts-expect-error Transaction lookup accepts only declared collections.
    tx.collection("missing");
    // @ts-expect-error Installed transactions reject text identities for numeric schemas.
    await tx.records.get("7");
    // @ts-expect-error Installed transaction cursors follow the same schema.
    await tx.records.find().after("7");
  });
}
void contracts;

const collisionSchema = {
  collection: {id:t.string().required().primaryKey()},
  declareMaskPolicy: {id:t.string().required().primaryKey()},
  transaction: {id:t.string().required().primaryKey()},
  from: {id:t.string().required().primaryKey()},
  live: {id:t.string().required().primaryKey()},
  constructor: {id:t.string().required().primaryKey()},
  __platform: {id:t.string().required().primaryKey()},
  ["__proto__"]: {id:t.string().required().primaryKey()},
  migrations: {id:t.string().required().primaryKey()},
  openSubscription: {id:t.string().required().primaryKey()},
};

function collisionContracts(db: Db<typeof collisionSchema>) {
  void db.transaction(async tx => {
    await tx.collection("transaction").get("row");
    await tx.collection("collection").get("row");
    await tx.collection("from").get("row");
    await tx.collection("live").get("row");
  });
  void db.collection("transaction").find({id:"row"});
  void db.collection("collection").find({id:"row"});
  void db.collection("declareMaskPolicy").find({id:"row"});
  void db.collection("from").find({id:"row"});
  void db.collection("live").find({id:"row"});
  void db.collection("constructor").find({id:"row"});
  void db.collection("__platform").find({id:"row"});
  void db.collection("__proto__").find({id:"row"});
  void db.migrations.find({id:"row"});
  void db.openSubscription.find({id:"row"});
  // @ts-expect-error Name lookup accepts only declared collections.
  db.collection("missing");
}
void collisionContracts;
