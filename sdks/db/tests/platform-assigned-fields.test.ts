/**
 * Platform-assigned fields: `validateDoc` neither REQUIRES them of the caller
 * nor MATERIALISES a value for them, and it never touches the caller's object.
 *
 * These three are one property with three consequences, which is why they are
 * tested together. A field carrying an `assign` is one the PLATFORM computes
 * (`policies/confined-system-shape.inject.toml`), so:
 *
 *   - demanding it of the caller is wrong - `insert({ path: "/x" })` must work
 *     on a collection whose descriptor marks `id`/`created_at`/`updated_at`/
 *     `version` `required: true` with no default;
 *   - filling one in is wrong - a value the SDK invents would reach the SQL
 *     builder and pre-empt the generator the runtime is about to run;
 *   - and neither may be achieved by editing the document the caller handed us.
 *
 * The last is an operator constraint on this file specifically, and it had NO
 * test before this one. The earlier SDK satisfied the first two by DELETING the
 * system fields from the insert input (`stripRuntimeSystemFields`), which is
 * exactly the shape now forbidden: a validator that edits its input makes the
 * caller's object depend on whether it was validated, and makes a retry after a
 * failed insert observe a different document than the first attempt.
 *
 * The `assign` here is not written by hand - it is the charter's own binding,
 * reaching the FieldDef through the generated projection - so a charter that
 * stopped declaring one would fail these tests rather than quietly widening
 * what the caller must supply.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { validateDoc } from "../src/validate.js";
import { ValidationError } from "../src/errors.js";
import type { NormalizedSchema } from "../src/schema.js";
import {
  CONFINED_SYSTEM_SHAPE_ASSIGNMENTS,
  CONFINED_SYSTEM_SHAPE_COLUMN_NAMES,
} from "../src/generated/confined-system-shape.generated.js";

/**
 * A collection shaped like a real descriptor: the platform's columns marked
 * `required: true` with no default (which is what the migration fold emits),
 * plus one ordinary creator field.
 */
function descriptorLikeSchema(): NormalizedSchema {
  const schema: NormalizedSchema = {
    path: { type: "string", required: true },
  };
  for (const name of CONFINED_SYSTEM_SHAPE_COLUMN_NAMES) {
    const assign = CONFINED_SYSTEM_SHAPE_ASSIGNMENTS[name];
    schema[name] = { type: "string", required: true, ...(assign ? { assign } : {}) };
  }
  return schema;
}

describe("platform-assigned fields are not required of the caller", () => {
  test("the charter actually declares assignments (anti-vacuity)", () => {
    // Every assertion below is vacuous if the projection carries no bindings:
    // a schema with no `assign` anywhere would make the "not required" tests
    // fail rather than pass, but the "not materialised" ones would pass for the
    // wrong reason. Pin the input first.
    assert.ok(
      CONFINED_SYSTEM_SHAPE_COLUMN_NAMES.length > 0,
      "the charter projection carries no columns",
    );
    assert.ok(
      Object.keys(CONFINED_SYSTEM_SHAPE_ASSIGNMENTS).length > 0,
      "no charter column carries an `assign`; these tests would prove nothing",
    );
  });

  test("a document supplying only creator fields validates", () => {
    const schema = descriptorLikeSchema();
    assert.doesNotThrow(() => validateDoc({ path: "/hit/ready" }, schema));
  });

  test("no value is materialised for an assigned field", () => {
    const schema = descriptorLikeSchema();
    const out = validateDoc({ path: "/hit/ready" }, schema);
    assert.deepEqual(
      out,
      { path: "/hit/ready" },
      "the validated document must reach the native op carrying ONLY what the " +
        "caller supplied - an invented id or timestamp would pre-empt the runtime",
    );
    for (const name of Object.keys(CONFINED_SYSTEM_SHAPE_ASSIGNMENTS)) {
      assert.ok(!(name in out), `${name} must not be present at all, not even as undefined`);
    }
  });

  test("an explicit null for an assigned field is dropped, not forwarded", () => {
    // Otherwise the native op receives an explicit NULL for a NOT NULL column
    // whose value the platform was about to compute.
    const schema = descriptorLikeSchema();
    const out = validateDoc({ path: "/x", created_at: null, id: null }, schema);
    assert.deepEqual(out, { path: "/x" });
  });

  test("a field WITHOUT an assign is still required", () => {
    // The control: this proves the arm above is doing something narrower than
    // "stop requiring things". `path` carries no assignment and must still be
    // demanded.
    const schema = descriptorLikeSchema();
    assert.throws(
      () => validateDoc({}, schema),
      (err: unknown) => {
        assert.ok(err instanceof ValidationError);
        assert.match((err as Error).message, /path is required/);
        return true;
      },
    );
  });

  test("an assign beats a default rather than materialising the seed", () => {
    // `version` is the live instance: the charter gives it BOTH an
    // `assign = increment(1)` and a DDL `DEFAULT 1`, because the generator is
    // the normal path and the DDL default is the backstop for writes that never
    // reach the runtime. If the default arm won, the seed would be written into
    // the row and the runtime's bump would be pre-empted.
    const schema: NormalizedSchema = {
      version: {
        type: "number",
        required: true,
        default: 1,
        assign: { by: "increment(1)", on: "write" },
      },
    };
    const out = validateDoc({}, schema);
    assert.deepEqual(out, {}, "the DDL default must not be materialised over an assign");
  });
});

describe("validateDoc does not touch the document it is given", () => {
  test("the caller's object is unchanged for assigned fields", () => {
    const schema = descriptorLikeSchema();
    const doc = { path: "/x", created_at: null };
    const before = JSON.stringify(doc);
    const out = validateDoc(doc, schema);
    assert.equal(
      JSON.stringify(doc),
      before,
      "validateDoc mutated its input; the caller's object must survive validation " +
        "byte for byte, which is what the deleted stripRuntimeSystemFields did not do",
    );
    assert.notEqual(out, doc, "the result must be a fresh object, not the input");
    assert.ok("created_at" in doc, "the input still carries the key it was given");
    assert.ok(!("created_at" in out), "the result does not");
  });

  test("the caller's object is unchanged when validation FAILS", () => {
    // The failure path is the one a retry runs against, so an input edited
    // before the throw would make attempt two see a different document.
    const schema = descriptorLikeSchema();
    const doc = { created_at: null };
    const before = JSON.stringify(doc);
    assert.throws(() => validateDoc(doc, schema), ValidationError);
    assert.equal(JSON.stringify(doc), before);
  });
});
