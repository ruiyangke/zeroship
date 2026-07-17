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

test("explicit primary-key add/replace/drop records the exact wire shape", () => {
  let returnedFromAdd: unknown;
  let returnedFromReplace: unknown;
  let returnedFromDrop: unknown;

  const ops = record(() => {
    const unkeyed = table("unkeyed");
    returnedFromAdd = unkeyed.primaryKey().add({ columns: ["tenant_id", "id"] });

    const orders = table("orders", { schema: "sales" });
    returnedFromReplace = orders.primaryKey().replace({
      expectedColumns: ["id"],
      columns: ["tenant_id", "order_id"],
      dropIdentityFrom: ["id"],
    });
    returnedFromDrop = orders.primaryKey().drop({
      expectedColumns: ["tenant_id", "order_id"],
      dropIdentityFrom: ["order_id"],
    });

    assert.equal(returnedFromAdd, unkeyed, "add returns its parent table handle");
    assert.equal(returnedFromReplace, orders, "replace returns its parent table handle");
    assert.equal(returnedFromDrop, orders, "drop returns its parent table handle");
  });

  assert.deepEqual(ops, [
    {
      op: "alterPrimaryKey",
      table: "unkeyed",
      action: { kind: "add", columns: ["tenant_id", "id"] },
    },
    {
      op: "alterPrimaryKey",
      table: "orders",
      action: {
        kind: "replace",
        expectedColumns: ["id"],
        columns: ["tenant_id", "order_id"],
        dropIdentityFrom: ["id"],
      },
      schema: "sales",
    },
    {
      op: "alterPrimaryKey",
      table: "orders",
      action: {
        kind: "drop",
        expectedColumns: ["tenant_id", "order_id"],
        dropIdentityFrom: ["order_id"],
      },
      schema: "sales",
    },
  ]);
});

test("primary-key lifecycle regression: replace/drop/add exist and changeIdType does not", () => {
  const handle = table("orders") as unknown as Record<string, unknown>;
  assert.equal(typeof handle.primaryKey, "function");
  assert.equal(handle.changeIdType, undefined);

  const ops = record(() => {
    table("orders")
      .primaryKey()
      .replace({ expectedColumns: ["id"], columns: ["tenant_id", "order_id"] })
      .primaryKey()
      .drop({ expectedColumns: ["tenant_id", "order_id"], dropIdentityFrom: ["order_id"] })
      .primaryKey()
      .add({ columns: ["id"] });
  });

  assert.deepEqual(
    ops.map((op) => [op.op, op.action.kind]),
    [
      ["alterPrimaryKey", "replace"],
      ["alterPrimaryKey", "drop"],
      ["alterPrimaryKey", "add"],
    ],
  );
});

test("every primary-key column tuple is validated at the authoring boundary", () => {
  const rejects = (author: () => void, message: RegExp): void => {
    assert.throws(
      () => record(author),
      (error: any) => error.code === "OP_INVALID" && message.test(error.message),
    );
  };

  rejects(
    () => table("t").primaryKey().add({ columns: [] as any }),
    /add\(\{ columns \}\).*non-empty ordered/,
  );
  rejects(
    () => table("t").primaryKey().add({ columns: ["id", "id"] }),
    /add\(\{ columns \}\).*more than once/,
  );
  rejects(
    () =>
      table("t").primaryKey().replace({
        expectedColumns: [] as any,
        columns: ["next_id"],
      }),
    /replace\(\{ expectedColumns \}\).*non-empty ordered/,
  );
  rejects(
    () =>
      table("t").primaryKey().replace({
        expectedColumns: ["id"],
        columns: [""] as any,
      }),
    /replace\(\{ columns \}\)\[0\].*non-empty string/,
  );
  rejects(
    () =>
      table("t").primaryKey().replace({
        expectedColumns: ["id"],
        columns: ["next_id"],
        dropIdentityFrom: [] as any,
      }),
    /replace\(\{ dropIdentityFrom \}\).*non-empty ordered/,
  );
  rejects(
    () =>
      table("t").primaryKey().replace({
        expectedColumns: ["id"],
        columns: ["id"],
      }),
    /replace\(\{ columns \}\).*must change the ordered primary-key tuple/,
  );
  rejects(
    () =>
      table("t").primaryKey().replace({
        expectedColumns: ["id"],
        columns: ["next_id"],
        dropIdentityFrom: ["other"],
      }),
    /replace\(\{ dropIdentityFrom \}\).*not in expectedColumns/,
  );
  rejects(
    () => table("t").primaryKey().drop({ expectedColumns: [] as any }),
    /drop\(\{ expectedColumns \}\).*non-empty ordered/,
  );
  rejects(
    () =>
      table("t").primaryKey().drop({
        expectedColumns: ["id"],
        dropIdentityFrom: ["id", "id"],
      }),
    /drop\(\{ dropIdentityFrom \}\).*more than once/,
  );
  rejects(
    () =>
      table("t").primaryKey().drop({
        expectedColumns: ["id"],
        dropIdentityFrom: ["other"],
      }),
    /drop\(\{ dropIdentityFrom \}\).*not in expectedColumns/,
  );
});
