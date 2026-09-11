import { test } from "node:test";
import assert from "node:assert/strict";
import { t, type Collection, type RowInput, type UpdateExpression } from "@zeroship/db";
import { installSchema, normalizeSchema } from "@zeroship/bootstrap/install-schema";

const fields = {
  key: t.string().primaryKey().assigned({ by: "typedId", on: "insert" }).required(),
  revision: t.number().assigned({ by: "increment(1)", on: "write" }).required(),
  removed: t.timestamp().assigned({ by: "now", on: "delete" }),
  title: t.string().required(),
};

test("renamed keys and assignments drive SDK dispatch", async () => {
  const calls: Array<{ op: string; filter: unknown; options?: unknown }> = [];
  const row = { key: "entry_a", revision: 1, removed: null, title: "hello" };
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
  const normalized = normalizeSchema(fields);
  normalized.revision.concurrency = true;
  normalized.removed.softDelete = true;
  installSchema({}, native as never, { descriptor: {
    version: 2, collections: { entries: { fields: normalized,
      options: { softDelete: true, versioning: true, strictness: "strict" }, indexes: [] } },
  } } as never);
  const db = native as unknown as { entries: Collection<typeof fields> };
  const input: RowInput<typeof fields> = { title: "hello" };
  // @ts-expect-error Assignment fields cannot be overridden through typed updates.
  const forbidden: UpdateExpression<typeof fields> = { revision: 9 };
  void input; void forbidden;
  const result = await db.entries.get("entry_a");
  assert.equal(result.error, null);
  assert.equal(result.data?.key, "entry_a");
  assert.deepEqual(calls[0].filter, { key: { $in: ["entry_a"] } });
  await db.entries.delete("entry_a");
  await db.entries.restore("entry_a");
  assert.deepEqual(calls.slice(1), [
    { op: "delete", filter: { key: "entry_a" } },
    { op: "restore", filter: { key: "entry_a" } },
  ]);
  const conflict = await db.entries.update({ key: "entry_a", revision: 1 }, { title: "new" });
  assert.equal((conflict.error as { code?: string })?.code, "OPTIMISTIC_CONCURRENCY");
  await db.entries.find().paginate({ numItems: 1 });
  const last = calls.at(-1)!;
  assert.deepEqual((last.options as { orderBy: unknown }).orderBy, { key: 1 });
});
