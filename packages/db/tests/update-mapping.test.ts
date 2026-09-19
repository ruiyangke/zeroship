import { test } from "node:test";
import assert from "node:assert/strict";
import { mapUpdateOutbound } from "../../../crates/zeroship-data-v8/js/runtime/utils.js";
import { ValidationError } from "../src/errors.js";

test("update mapping refuses assignments that collide after column mapping", () => {
  for (const patch of [
    { balance: 1, $set: { balance: 2 } },
    { $inc: { balance: 1 }, $mul: { balance: 2 } },
    { balance: 1, BALANCE: 2 },
  ]) {
    assert.throws(() => mapUpdateOutbound(patch, field => field.toLowerCase()), ValidationError);
  }
});

test("explicit set data keeps operator-shaped JSON literal", () => {
  const value = { $inc: 2, description: "literal JSON" };
  const result = mapUpdateOutbound({ $set: { Payload: value }, $inc: { Balance: 1 } }, field => field.toLowerCase());
  assert.deepEqual(result, { $set: { payload: value }, $inc: { balance: 1 } });
  assert.equal((result.$set as Record<string, unknown>).payload, value);
});
