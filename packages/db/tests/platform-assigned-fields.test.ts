// Assignment validation uses the declared fixture schema and leaves caller input intact.
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { validateDoc } from "../../../crates/zeroship-data-v8/js/runtime/validate.js";
import { ValidationError } from "../src/errors.js";
import type { NormalizedSchema } from "../src/schema.js";
import { generatedSchema } from "./_install-helper.js";
const generatedColumnNames = Object.keys(generatedSchema);
const generatedAssignments = Object.fromEntries(
  Object.entries(generatedSchema).map(([name, builder]) => [name, builder.toFieldDef().assign!]),
);

// Omit defaults so the fixture exercises assignment metadata alone.
function descriptorLikeSchema(): NormalizedSchema {
  const schema: NormalizedSchema = {
    path: { type: "string", required: true },
  };
  for (const name of generatedColumnNames) {
    const assign = generatedAssignments[name];
    schema[name] = { type: "string", required: true, ...(assign ? { assign } : {}) };
  }
  return schema;
}

describe("platform-assigned fields are not required of the caller", () => {
  test("the fixture declares assignment generators", () => {
    assert.ok(generatedColumnNames.length > 0, "fixture must declare columns");
    for (const [name, assignment] of Object.entries(generatedAssignments)) {
      assert.ok(assignment, `${name} must declare its generator`);
    }
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
    for (const name of Object.keys(generatedAssignments)) {
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
    // Leave initialization to the database default for an assigned counter.
    const schema: NormalizedSchema = {
      revision: {
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
      "validateDoc must leave the caller's object unchanged",
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
