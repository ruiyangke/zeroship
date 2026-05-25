import { describe, test } from "node:test";
import assert from "node:assert/strict";

import { t } from "@zeroship/db";
import type { Collection } from "../src/collection.js";
import type { TxCollection, TxQuery } from "../src/db-types.js";
import type { Query } from "../src/query.js";
import { installSchemaForTest } from "./_install-helper.js";

type ExampleSchema = {
  title: string;
  secret: string;
  embedding: number[];
  location: string;
};

type ExpectNever<T extends never> = T;
type PublicCollectionKeys = Exclude<
  keyof Collection<ExampleSchema>,
  "_setReady" | "_setResolveCollection" | "_loadRelations" | "Id" | "RowInput"
>;
type PublicQueryKeys = Exclude<keyof Query<ExampleSchema>, "_exec">;

type _TxCollectionTracksCollection = ExpectNever<
  Exclude<PublicCollectionKeys, keyof TxCollection<ExampleSchema>>
>;
type _TxQueryTracksQuery = ExpectNever<
  Exclude<PublicQueryKeys, keyof TxQuery<ExampleSchema>>
>;

async function assertTxSurfaceTypes(tx: TxCollection<ExampleSchema>): Promise<void> {
  await tx.purge("doc_1");
  await tx.purgeMany({});
  await tx.restore("doc_1");
  await tx.restoreMany({});
  await tx.bulkUnmask(
    [{ id: "doc_1", columns: ["secret"] }],
    { actor: { role: "admin" } },
  );
  await tx.search({ text: "hello", limit: 1 });
  await tx.search({ vector: [0.1, 0.2], k: 1 });
  await tx.near({
    field: "location",
    point: { lat: 37.78, lng: -122.41 },
    radius: 50,
  });

  const page = await tx.find({}).sort({ id: 1 }).paginate({ numItems: 1 });
  const cursor: string = page.continueCursor;
  const rows: Array<{ title?: string }> = page.page;
  const done: boolean = page.isDone;

  void cursor;
  void rows;
  void done;
}

void assertTxSurfaceTypes;

const native = {
  registerModel: () => Promise.resolve(),
  transaction: async (cb: (raw: unknown) => unknown) => cb(undefined),
  collection(_name: string) {
    return {
      async findOne() {
        return null;
      },
      async find() {
        return [
          {
            id: "doc_1",
            title: "hello",
            secret: "masked",
            created_at: 0,
            updated_at: 0,
          },
        ];
      },
      async insert(doc: Record<string, unknown>) {
        return { id: "doc_1", created_at: 0, updated_at: 0, ...doc };
      },
    };
  },
} as unknown as ZeroshipDb;

const db = installSchemaForTest(
  {
    docs: {
      title: t.string().required(),
      secret: t.string(),
      embedding: t.array(t.number()),
      location: t.string(),
    },
  },
  { native, naming: { toColumn: (s) => s, toField: (s) => s } },
);

describe("TxCollection parity", () => {
  test("tx wrappers expose the documented Collection and Query methods", async () => {
    await db.transaction(async (tx) => {
      const expectedCollectionMethods = [
        "insert",
        "insertMany",
        "get",
        "exists",
        "find",
        "upsert",
        "update",
        "updateMany",
        "delete",
        "deleteMany",
        "purge",
        "purgeMany",
        "restore",
        "restoreMany",
        "count",
        "distinct",
        "aggregate",
        "bulkUnmask",
        "search",
        "near",
      ] as const;
      for (const method of expectedCollectionMethods) {
        assert.equal(typeof (tx.docs as Record<string, unknown>)[method], "function");
      }

      const query = tx.docs.find({}).sort({ id: 1 });
      const expectedQueryMethods = [
        "sort",
        "limit",
        "skip",
        "select",
        "after",
        "with",
        "paginate",
        "first",
        "unique",
        "last",
        "then",
      ] as const;
      for (const method of expectedQueryMethods) {
        assert.equal(typeof (query as Record<string, unknown>)[method], "function");
      }

      const page = await query.paginate({ numItems: 1 });
      assert.equal(Array.isArray(page.page), true);
      assert.equal(typeof page.continueCursor, "string");
      assert.equal(typeof page.isDone, "boolean");
      return null;
    });
  });
});
