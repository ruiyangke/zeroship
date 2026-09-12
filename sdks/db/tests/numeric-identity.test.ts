import { test } from "node:test";
import assert from "node:assert/strict";
import { Collection } from "../src/collection.js";
import { loadRelations } from "../src/collection/relations.js";
import { t, type InferId, type PlainObject } from "../src/types.js";
import type { NativeDb } from "../src/native.js";

const fields = { id: t.bigInt().required().primaryKey(), label: t.string() };

function fixture() {
  const calls: { method: string; filter: PlainObject }[] = [];
  const record = (id: unknown) => ({ id: typeof id === "bigint" && id <= 100n ? Number(id) : id, label: "row" });
  const mutate = (method: string) => async (filter: PlainObject) => {
    calls.push({ method, filter });
    return record(filter.id);
  };
  const native = { collection: () => ({
    find: async (filter: PlainObject) => {
      calls.push({ method: "find", filter });
      const ids = (filter.id as { $in?: unknown[] })?.$in ?? [filter.id];
      return ids.filter(id => id !== 99).map(record);
    },
    update: mutate("update"), delete: mutate("delete"),
    purge: mutate("purge"), restore: mutate("restore"),
    bulkUnmask: async (items: { rowPk: string }[]) => ({
      results: Object.fromEntries(items.map(item => [item.rowPk, { label: "private" }])),
    }),
  }) } as unknown as NativeDb;
  const collection = new Collection<typeof fields, "records">("records", {
    id: { type: "bigInt", required: true, primaryKey: true }, label: { type: "string" },
  }, native);
  return { collection, calls };
}

test("numeric get batches preserve identity and match bigint decoding", async () => {
  const { collection, calls } = fixture();
  const large = 9007199254740993n;
  const results = await Promise.all([collection.get(0), collection.get(7), collection.get(7n), collection.get(large), collection.get(99)]);
  for (const result of results) assert.equal(result.error, null);
  assert.deepEqual(results.map(result => result.data?.id ?? null), [0, 7, 7, large, null]);
  assert.deepEqual(calls, [{ method: "find", filter: { id: { $in: [0, 7, large, 99] } } }]);
  const single = await collection.get(large, { select: ["id"] });
  assert.equal(single.error, null);
  assert.equal(single.data?.id, large);
  assert.deepEqual(calls.at(-1)?.filter, { id: large });
});

for (const method of ["update", "delete", "purge", "restore"] as const) {
  test(`${method} accepts numeric identity shorthand`, async () => {
    const { collection, calls } = fixture();
    for (const id of [0, 9007199254740993n]) {
      const result = method === "update" ? await collection.update(id, { label: "changed" }) : await collection[method](id);
      assert.equal(result.error, null);
      assert.equal(result.data?.id, id);
      assert.deepEqual(calls.at(-1), { method, filter: { id } });
    }
  });
}

test("bulk unmask returns keys in the caller's numeric representation", async () => {
  const { collection } = fixture();
  const ids = [7, 7n, 9007199254740993n];
  const result = await collection.bulkUnmask(ids.map(id => ({ id, columns: ["label"] })), { actor: { kind: "support", id: "usr_reader" } });
  assert.equal(result.error, null);
  assert.deepEqual([...result.data!.keys()], ids);
  for (const id of ids) assert.deepEqual(result.data!.get(id), { label: "private" });
});

for (const [type, ids] of [["string", ["7"]], ["int", [0, 7]], ["bigInt", [7n, 9007199254740993n]]] as const) {
  test(`relations load ${type} references without converting wire values`, async () => {
    const rows: PlainObject[] = [...ids, ids[0], null].map(parentId => ({ parentId }));
    const queries: unknown[] = [];
    await loadRelations({
      _name: "children", _schema: { parentId: { type, refTarget: "parents", refColumn: "id" } },
      _resolveCollection: () => ({
        _schema: { id: { type, required: true, primaryKey: true } },
        find: async filter => {
          queries.push(filter);
          return { data: ids.map(id => ({ id: id === 7n ? 7 : id })), error: null };
        },
      }),
    }, rows, { parentId: true });
    assert.deepEqual(queries, [{ id: { $in: [...ids] } }]);
    assert.deepEqual(rows.map(row => row.parentId), [...ids, ids[0]].map(id => ({ id: id === 7n ? 7 : id })).concat([null] as never));
  });
}

function contracts(collection: Collection<typeof fields, "records">) {
  const id = 7 as InferId<typeof collection>;
  const numeric: number | bigint = id;
  void collection.get(id);
  // @ts-expect-error Numeric identities do not accept text shorthand.
  void collection.get("7");
  // @ts-expect-error Mutations use the same identity type as reads.
  void collection.update("7", { label: "changed" });
  void numeric;
}
void contracts;
