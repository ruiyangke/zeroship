// The migration DSL and the runtime `db` schema share
// ONE column-type lexicon. These tests pin the single-source bridge
// (`colTypeFromDbField` / `fromDb`). The legacy runtime-schema `dbType.ref`
// carrier remains available for compatibility, while new migration references
// use an explicit physical type plus `.references(table, column)`.

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  colTypeFromDbField,
  dbType as dbT,
  fromDb,
  t,
  table,
  UnsupportedColTypeError,
} from "../src/index.js";
import { __begin, __drain } from "../src/ops.js";

/** The dialect-neutral `ColType` the migration `t.*` records for a column — read
 *  off the impl's `_type` brand (the exact field `.create()`/`.column().add()` lower). */
function migrateColType(def: unknown): unknown {
  return (def as { _type: unknown })._type;
}

test("ONE lexicon: a db field reduces to the same ColType the migration t.* produces", () => {
  // `dbT.string()` (no bounded-length contract) reduces to the unbounded `"text"`
  // ColType — identical rendering to the retired bare `string` ColType. A bounded
  // string is authored explicitly with `t.string({ length })` → `{string:{length}}`.
  assert.deepEqual(colTypeFromDbField(dbT.string()), "text");
  assert.deepEqual(colTypeFromDbField(dbT.boolean()), migrateColType(t.boolean()));
  assert.deepEqual(colTypeFromDbField(dbT.timestamp()), migrateColType(t.timestamp()));
  assert.deepEqual(colTypeFromDbField(dbT.json()), migrateColType(t.json()));
  assert.deepEqual(colTypeFromDbField(dbT.bytes()), migrateColType(t.bytes()));
  assert.deepEqual(colTypeFromDbField(dbT.geoPoint()), migrateColType(t.geoPoint()));
  // `t.double()` (a db float) maps to the neutral `double` ColType.
  assert.deepEqual(colTypeFromDbField(dbT.double()), migrateColType(t.double()));
  // A typed-id db field reduces to the bounded `string(36)` neutral storage.
  assert.deepEqual(colTypeFromDbField(dbT.typedId("post")), { string: { length: 36 } });
});

test("the legacy dbType.ref bridge remains distinct from typed migration references", () => {
  const fromSchema = colTypeFromDbField(dbT.ref("users"));
  assert.deepEqual(fromSchema, { ref: { references: "users" } });
  assert.equal(
    migrateColType(t.text().references("users", "id")),
    "text",
    "a typed migration reference preserves its explicit local storage",
  );
});

test("ONE lexicon: a pgvector field carries its dims through the shared ColType", () => {
  assert.deepEqual(colTypeFromDbField(dbT.vector(1536)), { vector: { vector: 1536 } });
  assert.deepEqual(colTypeFromDbField(dbT.vector(8)), migrateColType(t.vector({ dimensions: 8 })));
});

test("ONE lexicon: an encrypted column reduces to its plaintext ColType plus the encrypted facet", () => {
  // The type bridge sees only the PLAINTEXT type; encryption is a FACET, and
  // `fromDb` lifts it onto the recorded column alongside that plaintext type.
  assert.deepEqual(colTypeFromDbField(dbT.string().encrypted()), "text");
  assert.deepEqual(colTypeFromDbField(dbT.double().encrypted()), "double");
  __begin();
  table("accounts").create({ columns: { secret: fromDb(dbT.string().encrypted()) } });
  const ops = __drain();
  assert.equal(ops[0].columns[0].type, "text");
  assert.equal(ops[0].columns[0].encrypted, true);
});

test("fromDb keeps the legacy dbType.ref carrier and required facet", () => {
  __begin();
  table("posts").create({ columns: { author_id: fromDb(dbT.ref("users").required()) } });
  const viaSchema = __drain();

  assert.deepEqual(viaSchema[0].columns[0].type, { ref: { references: "users" } });
  assert.equal(viaSchema[0].columns[0].nullable, false, "required() → notNull carried over");
});

test("fromDb carries .unique() over from the db field", () => {
  __begin();
  table("u").create({ columns: { email: fromDb(dbT.string().unique()) } });
  const ops = __drain();
  assert.equal(ops[0].columns[0].unique, true);
});

test("t.actor() bridges as its string storage type", () => {
  assert.equal(colTypeFromDbField(dbT.actor()), "text");
});

test("every storage-backed db token reduces to a neutral ColType", () => {
  const asField = (type: string) =>
    ({ type } as unknown as Parameters<typeof colTypeFromDbField>[0]);

  assert.equal(colTypeFromDbField(dbT.timestamp()), "timestamp");
  // Builder and descriptor both spell this token "timestamp" now; the raw field
  // proves the bridge accepts the token without a TypeBuilder wrapper.
  assert.equal(colTypeFromDbField(asField("timestamp")), "timestamp");
  assert.equal(colTypeFromDbField(dbT.calendarDate()), "date");
  assert.equal(colTypeFromDbField(dbT.int()), "int");
  assert.equal(colTypeFromDbField(dbT.bigInt()), "bigInt");
  // Descriptor-only tokens: the runtime descriptor spells integer widths and
  // float precision that no `t.*` factory authors, so they need a raw field.
  assert.equal(colTypeFromDbField(asField("int")), "int");
  assert.equal(colTypeFromDbField(asField("integer")), "int");
  assert.equal(colTypeFromDbField(asField("float")), "double");
});

test("a non-storage db type (object/union/array/...) is a hard structured boundary, never silent", () => {
  for (const make of [
    () => dbT.object({ a: dbT.string() }),
    () => dbT.array(dbT.string()),
    () => dbT.literal("x"),
  ]) {
    assert.throws(
      () => colTypeFromDbField(make()),
      (e: unknown) => {
        assert.ok(e instanceof UnsupportedColTypeError, "is the structured boundary error");
        assert.equal((e as UnsupportedColTypeError).code, "COLTYPE_UNSUPPORTED");
        return true;
      },
    );
  }
});
