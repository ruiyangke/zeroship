import { test } from "node:test";
import assert from "node:assert/strict";
import { t, type RowId, type RowInput, type UpdateExpression } from "../src/types.js";

const fields = { id: t.string().required().primaryKey(), label: t.string().required() };

function contracts() {
  const insert: RowInput<typeof fields> = { id: "manual", label: "value" };
  const update: UpdateExpression<typeof fields> = { label: "changed" };
  // @ts-expect-error Identity is insertable, but cannot be changed.
  const changed: UpdateExpression<typeof fields> = { id: "changed" };
  // @ts-expect-error Document operators cannot change identity either.
  const set: UpdateExpression<typeof fields> = { $set: { id: "changed" } };
  void [insert, update, changed, set];
  const composite = { app: t.string().required().primaryKey(), generation:t.bigInt().required().primaryKey(), id:t.string() };
  const ordinary: UpdateExpression<typeof composite> = { id:"editable" };
  // @ts-expect-error Every declared key component is immutable.
  const move: UpdateExpression<typeof composite> = { app:"other" };
  // @ts-expect-error Arithmetic operators cannot change a key component.
  const increment: UpdateExpression<typeof composite> = { $inc:{generation:1} };
  void [ordinary, move, increment];
  const numeric = { key:t.bigInt().required().primaryKey(), id:t.string() };
  const numericId: RowId<typeof numeric> = 7n;
  // @ts-expect-error The ordinary id field does not supply the key type.
  const textId: RowId<typeof numeric> = "ordinary";
  // @ts-expect-error Composite keys cannot use scalar identity helpers.
  const incomplete: RowId<typeof composite> = "app";
  void [numericId, textId, incomplete];
  const chained = { key:t.bigInt().primaryKey().required().default(1n), id:t.string() };
  const generatedKey: RowId<typeof chained> = 2n;
  // @ts-expect-error Builder methods preserve the immutable primary-key brand.
  const rewritten: UpdateExpression<typeof chained> = {key:2n};
  // @ts-expect-error Builder methods preserve the named key's scalar type.
  const incorrect: RowId<typeof chained> = "ordinary";
  void [generatedKey, rewritten, incorrect];
}

test("manual identity remains an insert input", () => {
  assert.equal(fields.id.toFieldDef().primaryKey, true);
  assert.equal(fields.id.toFieldDef().assign, undefined);
  void contracts;
});
