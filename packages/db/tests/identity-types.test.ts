import { test } from "node:test";
import assert from "node:assert/strict";
import { t, type RowInput, type UpdateExpression } from "../src/types.js";

const fields = { id: t.string().required().primaryKey(), label: t.string().required() };

function contracts() {
  const insert: RowInput<typeof fields> = { id: "manual", label: "value" };
  const update: UpdateExpression<typeof fields> = { label: "changed" };
  // @ts-expect-error Identity is insertable, but cannot be changed.
  const changed: UpdateExpression<typeof fields> = { id: "changed" };
  // @ts-expect-error Document operators cannot change identity either.
  const set: UpdateExpression<typeof fields> = { $set: { id: "changed" } };
  void [insert, update, changed, set];
}

test("manual identity remains an insert input", () => {
  assert.equal(fields.id.toFieldDef().primaryKey, true);
  assert.equal(fields.id.toFieldDef().assign, undefined);
  void contracts;
});
