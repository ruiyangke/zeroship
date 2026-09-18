import { test } from "node:test";
import assert from "node:assert/strict";
import { t, type InferFieldDef, type RowInput, type UpdateExpression } from "../src/types";
import { validateDoc } from "../../../crates/zeroship-data-v8/js/runtime/validate";

test("JSON fields infer every JSON root for inserts and updates", () => {
  const fields = { payload: t.json() };
  const values: InferFieldDef<typeof fields.payload>[] = [
    { nested: ["tag", { enabled: true }] }, ["alpha", "beta"], "text", 42, false, null,
  ];
  for (const payload of values) {
    const insert: RowInput<typeof fields> = { payload };
    const update: UpdateExpression<typeof fields> = { $set: { payload } };
    assert.deepEqual(validateDoc(insert, { payload: fields.payload.toFieldDef() }), insert);
    assert.deepEqual(update.$set, insert);
  }

  // @ts-expect-error JSON fields do not contain executable values
  const callback: RowInput<typeof fields> = { payload: { nested: [() => 1] } };
  // @ts-expect-error JSON has no bigint representation
  const bigint: UpdateExpression<typeof fields> = { payload: 1n };
  void callback;
  void bigint;
});
