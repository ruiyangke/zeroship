import assert from "node:assert/strict";
import { test } from "node:test";

import { table } from "../src/index.js";
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

test("synchronizeIdentity records canonical import intent and returns the table handle", () => {
  let returned: unknown;
  const orders = table("orders", { schema: "sales" });
  const ops = record(() => {
    returned = orders.column("id").synchronizeIdentity({
      writesQuiesced: "orders_import_window",
    });
  });

  assert.equal(returned, orders);
  assert.deepEqual(ops, [
    {
      op: "synchronizeIdentity",
      table: "orders",
      column: "id",
      writesQuiesced: "orders_import_window",
      schema: "sales",
    },
  ]);
});

test("synchronizeIdentity requires a named non-whitespace writesQuiesced assertion", () => {
  for (const args of [undefined, {}, { writesQuiesced: "" }, { writesQuiesced: "   " }]) {
    assert.throws(
      () =>
        record(() => {
          table("orders").column("id").synchronizeIdentity(args as any);
        }),
      (error: any) =>
        error.code === "OP_INVALID" && /writesQuiesced|must be an object/.test(error.message),
    );
  }
});

test("synchronizeIdentity regression surface exists on ColumnRef", () => {
  record(() => {
    const column = table("orders").column("id") as unknown as Record<string, unknown>;
    assert.equal(typeof column.synchronizeIdentity, "function");
    (column.synchronizeIdentity as (args: { writesQuiesced: string }) => unknown)({
      writesQuiesced: "regression_window",
    });
  });
});
