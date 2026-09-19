import { test } from "node:test";
import assert from "node:assert/strict";
import { t, type Collection, type RowInput, type UpdateExpression } from "../src/index.js";
import { installSchema } from "../../../crates/zeroship-data-v8/js/testing.js";
import { fieldsOf } from "./_install-helper.js";

const fields = {
  id: t.string().primaryKey().assigned({ by: "typedId", on: "insert" }).required(),
  revision: t.double().assigned({ by: "increment(1)", on: "write" }).required(),
  removed: t.timestamp().assigned({ by: "now", on: "delete" }),
  title: t.string().required(),
};

test("id and renamed assignments drive SDK dispatch", async () => {
  const calls: Array<{ op: string; filter: unknown; options?: unknown }> = [];
  const row = { id: "entry_a", revision: 1, removed: null, title: "hello" };
  const native = {
    transaction: async (callback: (raw: unknown) => unknown) => callback(undefined),
    collection: () => ({
      find: async (filter: unknown, options: unknown) => {
        calls.push({ op: "find", filter, options });
        return [row];
      },
      update: async (filter: unknown) => { calls.push({ op: "update", filter }); return null; },
      delete: async (filter: unknown) => { calls.push({ op: "delete", filter }); return row; },
      restore: async (filter: unknown) => { calls.push({ op: "restore", filter }); return row; },
    }),
  };
  const normalized = fieldsOf(fields);
  normalized.revision.concurrency = true;
  normalized.removed.softDelete = true;
  installSchema(native as never, {
    collections: { entries: { fields: normalized, indexes: [] } },
  } as never);
  const db = native as unknown as { entries: Collection<typeof fields> };
  const input: RowInput<typeof fields> = { title: "hello" };
  // @ts-expect-error Assignment fields cannot be overridden through typed updates.
  const forbidden: UpdateExpression<typeof fields> = { revision: 9 };
  void input; void forbidden;
  const result = await db.entries.get("entry_a");
  assert.equal(result.error, null);
  assert.equal(result.data?.id, "entry_a");
  assert.deepEqual(calls[0].filter, { id: { $in: ["entry_a"] } });
  await db.entries.delete("entry_a");
  await db.entries.restore("entry_a");
  assert.deepEqual(calls.slice(1), [
    { op: "delete", filter: { id: "entry_a" } },
    { op: "restore", filter: { id: "entry_a" } },
  ]);
  const conflict = await db.entries.update({ id: "entry_a", revision: 1 }, { title: "new" });
  assert.equal((conflict.error as { code?: string })?.code, "OPTIMISTIC_CONCURRENCY");
  await db.entries.find().paginate({ numItems: 1 });
  const last = calls.at(-1)!;
  assert.deepEqual((last.options as { orderBy: unknown }).orderBy, { id: 1 });
});
