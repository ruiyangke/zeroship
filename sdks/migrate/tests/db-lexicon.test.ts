// PR5 goal (A) — the migration DSL and the runtime `@zeroship/db` schema share
// ONE column-type lexicon. These tests pin the single-source bridge
// (`colTypeFromDbField` / `fromDb`): a `@zeroship/db` `t.*` field reduces to the
// IDENTICAL dialect-neutral `ColType` the migration DSL's own `t.*` produces, so
// a `t.ref("users")` FK declared in a live schema lowers the same way an `{ref}`
// migration column does. Names stay plain strings — never live-schema-bound.

import assert from "node:assert/strict";
import { test } from "node:test";

import { t as dbT } from "@zeroship/db";

import { colTypeFromDbField, fromDb, t, table, UnsupportedColTypeError } from "../src/index.js";
import { __begin, __drain } from "../src/ops.js";

/** The dialect-neutral `ColType` the migration `t.*` records for a column — read
 *  off the impl's `_type` brand (the exact field `.create()`/`.column().add()` lower). */
function migrateColType(def: unknown): unknown {
  return (def as { _type: unknown })._type;
}

test("ONE lexicon: a @zeroship/db field reduces to the same ColType the migration t.* produces", () => {
  // `dbT.string()` reduces to the neutral `"string"` ColType (the migration
  // lexicon's canonical text type is `t.text()`→`"text"`; the `string` token is a
  // distinct wire variant only the db bridge still produces, §7 alias removal).
  assert.deepEqual(colTypeFromDbField(dbT.string()), "string");
  assert.deepEqual(colTypeFromDbField(dbT.boolean()), migrateColType(t.boolean()));
  assert.deepEqual(colTypeFromDbField(dbT.timestamp()), migrateColType(t.timestamp()));
  assert.deepEqual(colTypeFromDbField(dbT.json()), migrateColType(t.json()));
  assert.deepEqual(colTypeFromDbField(dbT.bytes()), migrateColType(t.bytes()));
  assert.deepEqual(colTypeFromDbField(dbT.geoPoint()), migrateColType(t.geoPoint()));
  // `t.number()` (a db float) maps to the neutral `double` ColType.
  assert.deepEqual(colTypeFromDbField(dbT.number()), migrateColType(t.double()));
  // `t.id(...)` reduces to the neutral `uuid` ColType (the typed_id PK candidate).
  assert.equal(colTypeFromDbField(dbT.id("post")), "uuid");
});

test("ONE lexicon: a t.ref FK in the schema lowers the same {ref} ColType as the migration DSL", () => {
  const fromSchema = colTypeFromDbField(dbT.ref("users"));
  const fromMigration = migrateColType(t.ref("users"));
  assert.deepEqual(fromSchema, { ref: { references: "users" } });
  assert.deepEqual(fromSchema, fromMigration, "schema FK and migration FK share one ColType");
});

test("ONE lexicon: a pgvector field carries its dims through the shared ColType", () => {
  assert.deepEqual(colTypeFromDbField(dbT.vector(1536)), { vector: { vector: 1536 } });
  assert.deepEqual(colTypeFromDbField(dbT.vector(8)), migrateColType(t.vector(8)));
});

test("ONE lexicon: an encrypted column reduces to the recursive `encrypted` ColType arm", () => {
  // db `t.encrypted({ wraps: t.number() })` → neutral { encrypted: { of: <inner> } }.
  assert.deepEqual(colTypeFromDbField(dbT.encrypted({ wraps: dbT.number() })), {
    encrypted: { of: "double" },
  });
  assert.deepEqual(colTypeFromDbField(dbT.encrypted()), { encrypted: { of: "string" } });
});

test("fromDb lifts a live-schema field into a migration column on the SAME ColType path", () => {
  // A db `t.ref("users").required()` → a migration column with the {ref} ColType +
  // notNull carried over, recorded byte-identically to a hand-written migration column.
  __begin();
  table("posts").create({ columns: { author_id: fromDb(dbT.ref("users").required()) } });
  const viaSchema = __drain();

  __begin();
  table("posts").create({ columns: { author_id: t.ref("users").notNull() } });
  const viaMigration = __drain();

  assert.deepEqual(viaSchema[0].columns, viaMigration[0].columns);
  assert.deepEqual(viaSchema[0].columns[0].type, { ref: { references: "users" } });
  assert.equal(viaSchema[0].columns[0].nullable, false, "required() → notNull carried over");
});

test("fromDb carries .unique() over from the @zeroship/db field", () => {
  __begin();
  table("u").create({ columns: { email: fromDb(dbT.string().unique()) } });
  const ops = __drain();
  assert.equal(ops[0].columns[0].unique, true);
});

test("a non-storage db type (object/union/array/...) is a hard structured boundary, never silent", () => {
  for (const make of [
    () => dbT.object({ a: dbT.string() }),
    () => dbT.array(dbT.string()),
    () => dbT.calendarDate(),
    () => dbT.actor(),
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
