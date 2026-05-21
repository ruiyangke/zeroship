/**
 * Type-level assertions for the v2 relations generics.
 *
 * The runtime body of each test is trivial — what makes these "tests"
 * is `tsc --noEmit` (run by `pnpm build` / the test runner's TS step)
 * refusing to compile the file if the generics regress. Each `const _x:
 * SomeType = ...` is an assignability assertion: changing
 * `WithRelations`/`Collection`/`Query` so the joined field widens back
 * to `PlainObject` (or narrows to `never`) will fail to compile.
 *
 * The shapes we lock in:
 *   - `db.todos.find({}, { with: { userId: true } }).data[0].userId.email` is `string`
 *   - The chained `.with({ userId: true })` form propagates the AllSchemas
 *     parameter so the result matches the inline form
 *   - `tx.todos.find({}, { with: { userId: true } })` is also strong-typed
 *   - Without `with`, the row type stays `Row<S>` (no joined keys leak)
 *   - The `Id<TargetName>` brand survives the join (the joined row carries `id: number`)
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { installSchemaForTest } from "./_install-helper.js";
import { t } from "@zeroship/db";

type AnyRec = Record<string, unknown>;

const native = {
  registerModel: () => Promise.resolve(),
  beginTransaction: async () => ({
    commit: async () => undefined,
    rollback: async () => undefined,
  }),
  collection(_name: string) {
    return {
      async findOne(_filter: AnyRec, _opts: AnyRec) { return null; },
      async find(_filter: AnyRec, _opts: AnyRec) { return []; },
      async insert(doc: AnyRec) { return doc; },
    };
  },
} as unknown as ZeroshipDb;

const db = installSchemaForTest(
  {
    users: {
      email: t.string().required().unique(),
      name: t.string().required(),
    },
    projects: {
      name: t.string().required(),
    },
    todos: {
      userId: t.ref("users"),
      projectId: t.ref("projects").required(),
      title: t.string().required(),
    },
  },
  { native, naming: { toColumn: (s) => s, toField: (s) => s } },
);

describe("relations type-level: inline find(filter, { with })", () => {
  test("data[0].userId is Row<usersSchema> | null — .email / .name / .id all typecheck", async () => {
    const { data } = await db.todos.find({}, { with: { userId: true } });
    if (!data || data.length === 0) return;
    const row = data[0];
    // The joined key narrows to the target row + null. Each typed access
    // below is an assignability check — uncomment any line and replace
    // its annotation with the wrong type to verify the assertion fires.
    if (row.userId !== null) {
      const email: string = row.userId.email;
      const name: string = row.userId.name;
      const id: number = row.userId.id;
      const createdAt: number = row.userId.createdAt;
      const updatedAt: number = row.userId.updatedAt;
      assert.equal(typeof email, "string");
      assert.equal(typeof name, "string");
      assert.equal(typeof id, "number");
      assert.equal(typeof createdAt, "number");
      assert.equal(typeof updatedAt, "number");
    }
  });

  test("two relations: both joined fields resolve to their target Row", async () => {
    const { data } = await db.todos.find(
      {},
      { with: { userId: true, projectId: true } },
    );
    if (!data || data.length === 0) return;
    const row = data[0];
    if (row.userId !== null) {
      const _email: string = row.userId.email;
      assert.equal(typeof _email, "string");
    }
    if (row.projectId !== null) {
      const _projectName: string = row.projectId.name;
      assert.equal(typeof _projectName, "string");
    }
  });
});

describe("relations type-level: chainable .with(...)", () => {
  test(".with({ userId: true }) propagates AllSchemas — joined Row<usersSchema>", async () => {
    const { data } = await db.todos.find({}).with({ userId: true });
    if (!data || data.length === 0) return;
    const row = data[0];
    if (row.userId !== null) {
      const email: string = row.userId.email;
      assert.equal(typeof email, "string");
    }
  });

  test("Query.paginate honours .with(...) at the type layer too", async () => {
    const { data } = await db.todos
      .find({})
      .sort({ id: 1 })
      .with({ userId: true })
      .paginate({ cursor: null, numItems: 10 });
    if (!data || data.page.length === 0) return;
    const row = data.page[0];
    if (row.userId !== null) {
      const email: string = row.userId.email;
      assert.equal(typeof email, "string");
    }
  });
});

describe("relations type-level: TxCollection / TxQuery propagation", () => {
  test("tx.todos.find({}, { with: { userId: true } }) — joined Row<usersSchema>", async () => {
    await db.transaction(async (tx) => {
      const rows = await tx.todos.find({}, { with: { userId: true } });
      if (rows.length > 0) {
        const row = rows[0];
        if (row.userId !== null) {
          const email: string = row.userId.email;
          assert.equal(typeof email, "string");
        }
      }
      return null;
    });
  });

  test("tx.todos.find({}).with({ userId: true }) chainable — joined Row<usersSchema>", async () => {
    await db.transaction(async (tx) => {
      const rows = await tx.todos.find({}).with({ userId: true });
      if (rows.length > 0) {
        const row = rows[0];
        if (row.userId !== null) {
          const email: string = row.userId.email;
          assert.equal(typeof email, "string");
        }
      }
      return null;
    });
  });

  test("tx.todos.get(1, { with: { userId: true } }) — joined Row<usersSchema>", async () => {
    await db.transaction(async (tx) => {
      const row = await tx.todos.get(1, { with: { userId: true } });
      if (row !== null && row.userId !== null) {
        const email: string = row.userId.email;
        assert.equal(typeof email, "string");
      }
      return null;
    });
  });
});

describe("relations type-level: no `with` → row shape unchanged", () => {
  test("find({}) without `with` keeps userId as Id<\"users\"> (no joined key leaks)", async () => {
    const { data } = await db.todos.find({});
    if (!data || data.length === 0) return;
    const row = data[0];
    // userId is still the FK brand `Id<"users"> | undefined` — assignable
    // to `number | undefined` because Id<T> = number & {...}. If the row
    // type had been polluted with `Row<usersSchema>`, this line would
    // refuse to typecheck.
    const idOrUndef: number | undefined = row.userId;
    assert.equal(idOrUndef === undefined || typeof idOrUndef === "number", true);
  });

  test("get(1) without `with` keeps the bare Row<S> shape", async () => {
    const { data } = await db.todos.get(1);
    if (!data) return;
    const idOrUndef: number | undefined = data.userId;
    assert.equal(idOrUndef === undefined || typeof idOrUndef === "number", true);
  });
});
