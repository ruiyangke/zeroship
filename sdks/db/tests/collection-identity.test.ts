import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { Collection } from "@zeroship/db";
import { validateCollectionIdentity, type NormalizedSchema } from "../src/schema.js";
import type { NativeDb } from "../src/native.js";
import { installSchema, type RuntimeSchemaDescriptor } from "@zeroship/bootstrap/install-schema";

const id = { type: "string" as const, required: true, primaryKey: true };

test("collection identity is explicit and does not invent assignments", () => {
  const fields = { id, slug: { type: "string" as const, unique: true } };
  const native = {} as ConstructorParameters<typeof Collection>[2];
  assert.doesNotThrow(() => new Collection("entries", fields, native));
  assert.equal("assign" in fields.id, false);
  assert.deepEqual(Object.keys(fields), ["id", "slug"]);
});

test("creator and Rust validators share the identity corpus", () => {
  const corpus = JSON.parse(readFileSync(new URL("../../../tests/fixtures/data/collection-identity.json", import.meta.url), "utf8")) as {
    valid:Record<string, {fields:NormalizedSchema}>;
    invalid:Record<string, {fields:NormalizedSchema; error:string}>;
  };
  for (const sample of Object.values(corpus.valid)) assert.doesNotThrow(() => validateCollectionIdentity(sample.fields));
  for (const sample of Object.values(corpus.invalid)) assert.throws(() => validateCollectionIdentity(sample.fields), { message:sample.error });
});

test("scalar operations resolve named keys and compound keys require filters", async () => {
  const calls: unknown[] = [];
  const native = { collection:() => ({
    find:async (filter:unknown) => { calls.push(filter); return []; },
    update:async (filter:unknown) => { calls.push(filter); return null; },
    delete:async (filter:unknown) => { calls.push(filter); return null; },
    purge:async (filter:unknown) => { calls.push(filter); return null; },
    restore:async (filter:unknown) => { calls.push(filter); return null; },
  }) } as unknown as NativeDb;
  const named = new Collection("entries", { key:id, label:{type:"string"} }, native);
  for (const method of ["get", "delete", "purge", "restore"] as const) {
    assert.equal((await named[method]("record")).error, null);
    assert.deepEqual(calls.pop(), {key:"record"});
  }
  assert.equal((await named.update("record", {label:"changed"})).error, null);
  assert.deepEqual(calls.pop(), {key:"record"});
  assert.equal((await named.find().after("record")).error, null);
  assert.deepEqual(calls.pop(), {key:{$gt:"record"}});
  const compound = new Collection("entries", {tenant:id, key:id, label:{type:"string"}}, native);
  const getError = (await compound.get("record")).error as Error & {code:string};
  const cursorError = (await compound.find().after("record")).error as Error & {code:string};
  const pageError = (await compound.find().paginate({numItems:1})).error as Error & {code:string};
  assert.equal(getError.code, "COMPOSITE_KEY_FILTER_REQUIRED");
  assert.equal(cursorError.code, "COMPOSITE_KEY_FILTER_REQUIRED");
  assert.equal(pageError.code, "COMPOSITE_KEY_FILTER_REQUIRED");
  assert.equal(calls.length, 0);
  assert.equal((await compound.get({tenant:"a", key:"record"})).error, null);
  assert.deepEqual(calls.pop(), {tenant:"a", key:"record"});
});

test("pagination uses the declared named key as its tiebreaker", async () => {
  const calls: Array<{filter:unknown; options:ZeroshipDbFindOpts}> = [];
  const native = { collection:() => ({
    find:async (filter:unknown, options:ZeroshipDbFindOpts) => {
      calls.push({filter, options});
      return calls.length === 1 ? [
        {key:"a", label:"shared", id:"ordinary-z"},
        {key:"b", label:"shared", id:"ordinary-a"},
      ] : [];
    },
  }) } as unknown as NativeDb;
  const collection = new Collection("entries", {
    key:id, label:{type:"string"}, id:{type:"string"},
  }, native);
  const first = await collection.find().sort({label:1}).paginate({numItems:1});
  assert.equal(first.error, null);
  assert.deepEqual(calls[0].options.orderBy, {label:1, key:1});
  const second = await collection.find().sort({label:1}).paginate({
    numItems:1, cursor:first.data!.continueCursor,
  });
  assert.equal(second.error, null);
  assert.deepEqual(calls[1].filter, {$or:[
    {label:{$gt:"shared"}},
    {$and:[{label:"shared"}, {key:{$gt:"a"}}]},
  ]});
});

test("invalid identities fail before collections are published", () => {
  const invalidFields: Array<ConstructorParameters<typeof Collection>[1]> = [
    {},
    { id: { ...id, primaryKey: false } },
    { id: { type: "string" as const, primaryKey: true } },
    { id: { ...id, required: false } },
    { id: { ...id, assign: { by: "actor", on: "write" } } },
    { id: { ...id, assign: { by: "actor", on: "delete" } } },
  ];
  for (const fields of invalidFields) {
    const native = {} as ConstructorParameters<typeof Collection>[2];
    assert.throws(() => new Collection("invalid", fields, native), {
      code: "INVALID_COLLECTION_IDENTITY",
    });
    const collection = (fields: unknown) => ({
      fields, options: { softDelete: false, versioning: false }, indexes: [],
    });
    const descriptor = { version: 2, collections: {
      entries: collection({ id }), invalid: collection(fields),
    } } as RuntimeSchemaDescriptor;
    assert.throws(() => installSchema({}, native, { descriptor }), {
      code: "INVALID_COLLECTION_IDENTITY",
    });
    assert.equal("entries" in native, false);
    assert.equal("invalid" in native, false);
  }
});

test("creator collections accept named and compound primary keys", () => {
  const schemas: Array<ConstructorParameters<typeof Collection>[1]> = [{ key: id }, { tenant: id, key: id }];
  for (const fields of schemas) {
    const native = {} as ConstructorParameters<typeof Collection>[2];
    assert.doesNotThrow(() => new Collection("entries", fields, native));
  }
});
