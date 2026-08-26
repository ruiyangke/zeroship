// What the authoring surface can and cannot say about `notValid`, pinned at the
// recorder rather than argued from the type declarations.
//
// The engine refuses a create-time `notValid` in EITHER spelling. That refusal used
// to be asymmetric - `validate` refused only `Some(true)` while `lower` refused any
// `Some(_)` - so an IR carrying `{ notValid: false }` on a create-time constraint
// cleared validate and then died at lower with "validated createTable NOT VALID
// FOREIGN KEY reached lower", a message written for a validator bypass. Both gates
// now refuse the same set.
//
// This file establishes the OTHER half of that story, and it is the half that
// decides how much the asymmetry could ever have cost: the recorder does not emit
// the field on the create-time path at all. `create({ foreignKeys: [...] })` builds
// its constraint by enumerating fields explicitly, and `notValid` is not among them,
// so even an untyped caller passing it gets an envelope without it. The engine's
// refusal is therefore a guard on hand-crafted IR and on non-TypeScript hosts
// calling the addon directly - which is exactly what the refusal's own comment says
// it is for - and not a bug an author using this package could hit.
//
// It is pinned because it is load-bearing in both directions. If the create-time
// builder ever started forwarding `notValid`, this test fails and says so, and at
// that moment the engine-side refusal stops being defense-in-depth and becomes the
// only thing standing between an author and a create-time facet PostgreSQL silently
// discards (measured on 18.4: the statement is accepted and the constraint is stored
// with `convalidated = true`).

import assert from "node:assert/strict";
import { test } from "node:test";

import { t, table } from "../src/index.js";
import { __abort, __begin, __drain } from "../src/ops.js";

function record(up: () => void): any[] {
  __begin();
  try {
    up();
    return __drain();
  } catch (error) {
    __abort();
    throw error;
  }
}

/** A create whose table-level foreign key is handed `notValid`, cast away because
 *  `TableForeignKey` does not declare the field. A typed author cannot write this;
 *  the cast is what lets the test ask whether the RECORDER would carry it anyway. */
function createWithCreateTimeNotValid(notValid: boolean): () => void {
  return () => {
    table("parents").create({
      columns: { id: t.int().notNull() },
      primaryKey: ["id"],
    });
    table("children").create({
      columns: { id: t.int().notNull(), parent_id: t.int() },
      primaryKey: ["id"],
      foreignKeys: [
        {
          name: "children_parent_fkey",
          columns: ["parent_id"],
          references: { table: "parents", columns: ["id"] },
          notValid,
        } as unknown as never,
      ],
    });
  };
}

test("the create-time constraint builder does not carry notValid, in either spelling", () => {
  for (const spelling of [true, false]) {
    const ops = record(createWithCreateTimeNotValid(spelling));
    const create = ops.find((op) => op.op === "createTable" && op.name === "children");
    assert.ok(create, "the children createTable was recorded");
    assert.equal(
      JSON.stringify(create).includes("notValid"),
      false,
      `create({ foreignKeys }) must not record notValid:${spelling}; got ${JSON.stringify(create.constraints)}`,
    );
  }
});

test("the add path DOES carry notValid, which is what makes the omission above a real boundary", () => {
  // The control. Without it, the assertions above would also pass if the recorder
  // had dropped `notValid` everywhere, or if the field had been renamed - and the
  // engine's create-time refusal would be guarding a facet nothing could author.
  const ops = record(() => {
    table("children")
      .foreignKey("children_parent_fkey")
      .add({
        columns: ["parent_id"],
        references: { table: "parents", columns: ["id"] },
        notValid: true,
      });
  });
  const added = ops.find((op) => op.op === "addConstraint");
  assert.ok(added, "the addConstraint was recorded");
  assert.equal(
    added.constraint.kind.notValid,
    true,
    `foreignKey(name).add({ notValid: true }) must record the facet; got ${JSON.stringify(added.constraint)}`,
  );

  // And absence stays absent rather than becoming an explicit false, because the
  // wire image has to be byte-identical to the pre-facet one.
  const plain = record(() => {
    table("children")
      .foreignKey("children_parent_fkey")
      .add({
        columns: ["parent_id"],
        references: { table: "parents", columns: ["id"] },
      });
  }).find((op) => op.op === "addConstraint");
  assert.equal(
    "notValid" in plain.constraint.kind,
    false,
    `a plain add must omit the key entirely; got ${JSON.stringify(plain.constraint)}`,
  );
});
