/**
 * Relation-aware reads — `with: { fk: true }` eager-loads referenced rows.
 *
 * Covers:
 *   - happy path (one fk, joined row attached)
 *   - null fk → joined field is null
 *   - missing target row (fk points to a deleted row) → null
 *   - two relations in one call → single roundtrip per relation
 *   - repeated fk values dedupe to one IN clause
 *   - with: { bogus: true } (not a ref field) → rejects with clear error
 *   - with: { id: true } (not a ref field) → rejects similarly
 *   - inline `find(filter, { with })` and chainable `.with(...)` both work
 *   - Query.paginate honours `.with(...)` and dedupes the same way
 *   - inside db.transaction(...) — `tx.x.find(...).with(...)` joins on TX_CONN
 *   - type-level: `Awaited<...>.data[0].user` is `PlainObject | null`
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { installSchemaForTest } from "./_install-helper.js";
import { t, schema } from "@zeroship/db";
import { Query } from "../src/query.js";

type AnyRec = Record<string, unknown>;

/**
 * **P9 PR 1** — the SDK's `get()` now routes through `find` with
 * `{limit:1}` (the native `findOne` v8_method was removed). The mock
 * splits `find` calls into two buckets:
 *   - `findBatched`: the `{id: {$in: [...]}}` shape from the IdLoader
 *   - `findSingle`: the `{limit:1}` shape (formerly the `findOne` call)
 *
 * Old test assertions on `calls.findOne` migrate to `calls.findSingle`.
 */
type CallLog = {
  find: { collection: string; filter: AnyRec; opts: AnyRec }[];
  findBatched: { collection: string; filter: AnyRec; opts: AnyRec }[];
  findSingle: { collection: string; filter: AnyRec; opts: AnyRec }[];
};

/** Mock native that returns rows by table. The find handler understands
 *  `{ id: { $in: [...] } }` so the relation loader's batched IN works.
 *
 *  **P7 PR 3** — FK columns cascade to TEXT typed_id; the loader sends
 *  stringified ids on the wire. The mock accepts both shapes by
 *  stringifying on the way in (the row tables stay number-keyed for
 *  readability — JS object indexing coerces both `rows[1]` and
 *  `rows["1"]` to the same slot). */
function makeMock(
  tables: Record<string, Record<number, AnyRec>>,
): { native: ZeroshipDb; calls: CallLog } {
  const calls: CallLog = { find: [], findBatched: [], findSingle: [] };
  const native = {
    registerModel: () => Promise.resolve(),
    // P9 PR 3: native `transaction(callback)` orchestrator stub.
    transaction: async (cb: (raw: unknown) => unknown) => cb(undefined),
    collection(name: string) {
      return {
        async find(filter: AnyRec, opts: AnyRec) {
          calls.find.push({ collection: name, filter, opts });
          const rows = tables[name] ?? {};
          const idClause = filter.id as
            | { $in?: (string | number)[] }
            | string
            | number
            | undefined;
          const isBatched =
            idClause !== null &&
            typeof idClause === "object" &&
            Array.isArray((idClause as AnyRec).$in);
          if (isBatched) {
            calls.findBatched.push({ collection: name, filter, opts });
            const ids = (idClause as { $in: (string | number)[] }).$in;
            return ids
              .map((i) => (rows as Record<string, AnyRec>)[String(i)])
              .filter(Boolean);
          }
          if (opts && (opts as AnyRec).limit === 1) {
            calls.findSingle.push({ collection: name, filter, opts });
          }
          if (typeof idClause === "number" || typeof idClause === "string") {
            const r = (rows as Record<string, AnyRec>)[String(idClause)];
            return r ? [r] : [];
          }
          // Whole-table scan with optional field-eq filter.
          const out: AnyRec[] = [];
          for (const r of Object.values(rows)) {
            let ok = true;
            for (const [k, v] of Object.entries(filter)) {
              if (k.startsWith("$")) continue;
              if (r[k] !== v) { ok = false; break; }
            }
            if (ok) out.push(r);
          }
          return out;
        },
        async insert(_doc: AnyRec) { return _doc; },
      };
    },
  };
  return { native: native as unknown as ZeroshipDb, calls };
}

function makeDb(calls?: CallLog) {
  const tables: Record<string, Record<number, AnyRec>> = {
    users: {
      1: { id: 1, email: "alice@example.com", name: "Alice" },
      2: { id: 2, email: "bob@example.com", name: "Bob" },
    },
    projects: {
      10: { id: 10, name: "Apollo" },
      11: { id: 11, name: "Beacon" },
    },
    todos: {
      100: { id: 100, userId: 1, projectId: 10, title: "buy milk" },
      101: { id: 101, userId: 2, projectId: 10, title: "write tests" },
      102: { id: 102, userId: 1, projectId: 11, title: "deploy" },
      103: { id: 103, userId: null,           projectId: 10, title: "orphan" },
      // FK to a user that doesn't exist in the users table — missing target.
      104: { id: 104, userId: 9999, projectId: 11, title: "ghost" },
    },
  };
  const mock = makeMock(tables);
  if (calls) {
    calls.find = mock.calls.find;
    calls.findBatched = mock.calls.findBatched;
    calls.findSingle = mock.calls.findSingle;
  }
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
    { native: mock.native, naming: { toColumn: s => s, toField: s => s } },
  );
  return { db, calls: mock.calls };
}

describe("with: { fk: true } — relation-aware reads", () => {
  test("happy path: find(filter, { with: { userId: true } }) attaches the joined row", async () => {
    const { db, calls } = makeDb();
    // Project 10 has 3 todos in fixtures: 100 (userId=1), 101 (userId=2),
    // 103 (userId=null — exercises the null FK path on the same page).
    const { data, error } = await db.todos.find({ projectId: 10 }, { with: { userId: true } });
    assert.equal(error, null);
    assert.ok(data);
    assert.equal(data!.length, 3);
    const t100 = data!.find((r) => r.id === 100) as AnyRec;
    const t101 = data!.find((r) => r.id === 101) as AnyRec;
    const t103 = data!.find((r) => r.id === 103) as AnyRec;
    assert.deepEqual(t100.userId, { id: 1, email: "alice@example.com", name: "Alice" });
    assert.deepEqual(t101.userId, { id: 2, email: "bob@example.com", name: "Bob" });
    assert.equal(t103.userId, null, "null FK on the page stays null after join");

    // The relation fired exactly ONE find against `users` (batch IN of distinct ids).
    const userFinds = calls.find.filter((c) => c.collection === "users");
    assert.equal(userFinds.length, 1, "expected exactly one batched find against users");
    // **P7 PR 3** — FK columns are TEXT typed_ids on the wire; the
    // loader stringifies legacy-number FK values before the `$in`.
    const idClause = userFinds[0].filter.id as { $in: string[] };
    assert.deepEqual([...idClause.$in].sort(), ["1", "2"]);
  });

  test("chainable .with(...) on Query produces the same result", async () => {
    const { db } = makeDb();
    const { data } = await db.todos
      .find({ projectId: 10 })
      .with({ userId: true });
    assert.ok(data);
    const t100 = data!.find((r) => r.id === 100) as AnyRec;
    assert.deepEqual(t100.userId, { id: 1, email: "alice@example.com", name: "Alice" });
  });

  test("get(id, { with: { userId: true } }) attaches the joined row", async () => {
    const { db, calls } = makeDb();
    const { data, error } = await db.todos.get(100, { with: { userId: true } });
    assert.equal(error, null);
    assert.ok(data);
    assert.deepEqual((data as AnyRec).userId, { id: 1, email: "alice@example.com", name: "Alice" });

    // get(id) with `with` skips the DataLoader path and goes through
    // find with limit:1 (formerly findOne).
    assert.equal(calls.findSingle.length, 1, "get(id, {with}) bypasses the loader");
    assert.equal(calls.findSingle[0].collection, "todos");
  });

  test("null FK value → joined field is null", async () => {
    const { db } = makeDb();
    const { data } = await db.todos.get(103, { with: { userId: true } });
    assert.ok(data);
    assert.equal((data as AnyRec).userId, null);
  });

  test("missing target row → joined field is null", async () => {
    const { db } = makeDb();
    const { data } = await db.todos.get(104, { with: { userId: true } });
    assert.ok(data);
    assert.equal((data as AnyRec).userId, null);
  });

  test("two relations in one call → one roundtrip per relation", async () => {
    const { db, calls } = makeDb();
    const { data, error } = await db.todos.find(
      { projectId: 10 },
      { with: { userId: true, projectId: true } },
    );
    assert.equal(error, null);
    assert.ok(data);
    const t100 = data!.find((r) => r.id === 100) as AnyRec;
    assert.deepEqual(t100.userId, { id: 1, email: "alice@example.com", name: "Alice" });
    assert.deepEqual(t100.projectId, { id: 10, name: "Apollo" });

    const userFinds = calls.find.filter((c) => c.collection === "users");
    const projectFinds = calls.find.filter((c) => c.collection === "projects");
    assert.equal(userFinds.length, 1, "one find against users");
    assert.equal(projectFinds.length, 1, "one find against projects");
  });

  test("same FK referenced N times → deduped to one IN clause", async () => {
    const { db, calls } = makeDb();
    // todos 100 and 102 both have userId=1 — should dedupe to [1] in the IN.
    const { data } = await db.todos.find({}, { with: { userId: true } });
    assert.ok(data);
    const userFinds = calls.find.filter((c) => c.collection === "users");
    assert.equal(userFinds.length, 1);
    // **P7 PR 3** — typed_id string wire shape.
    const idClause = userFinds[0].filter.id as { $in: string[] };
    // Only the distinct non-null user ids should appear: 1 and 2 (104's 9999 too).
    const seen = new Set(idClause.$in);
    assert.equal(seen.size, idClause.$in.length, "ids must be deduped");
    assert.ok(seen.has("1"));
    assert.ok(seen.has("2"));
  });

  test("empty result set → no relation fetch fires", async () => {
    const { db, calls } = makeDb();
    // userId = 9999 doesn't exist on any todo (todos table key is id, not userId).
    // Use an impossible filter via id IN [] surrogate: count -based check below.
    const { data } = await db.todos.find({ id: -1 }, { with: { userId: true } });
    assert.deepEqual(data, []);
    const userFinds = calls.find.filter((c) => c.collection === "users");
    assert.equal(userFinds.length, 0, "no relation fetch on empty page");
  });

  test("with: { bogus: true } — clear error when field is unknown", async () => {
    const { db } = makeDb();
    const { data, error } = await db.todos.find({}, { with: { bogus: true } as { bogus: true } });
    assert.equal(data, null);
    assert.ok(error);
    assert.match(error!.message, /is not a t\.ref field/);
  });

  test("with: { id: true } — clear error when field is not a ref", async () => {
    const { db } = makeDb();
    // `id` is a real field but not a t.ref — must reject identically.
    const { data, error } = await db.todos.find({}, { with: { id: true } as { id: true } });
    assert.equal(data, null);
    assert.ok(error);
    assert.match(error!.message, /is not a t\.ref field/);
  });

  test("with: { title: true } — clear error when field exists but isn't a ref", async () => {
    const { db } = makeDb();
    const { data, error } = await db.todos.find({}, { with: { title: true } as { title: true } });
    assert.equal(data, null);
    assert.ok(error);
    assert.match(error!.message, /is not a t\.ref field/);
  });

  test("paginate({...}) honours `.with(...)`", async () => {
    const { db, calls } = makeDb();
    const { data, error } = await db.todos
      .find({})
      .sort({ id: 1 })
      .with({ userId: true })
      .paginate({ cursor: null, numItems: 3 });
    assert.equal(error, null);
    assert.ok(data);
    assert.equal(data!.page.length, 3);
    // First page is todos 100, 101, 102 — userIds 1, 2, 1.
    for (const r of data!.page as AnyRec[]) {
      const uid = r.userId;
      // Either a joined user object or null (id 103 has null FK; not in this page).
      assert.ok(uid === null || (typeof uid === "object" && uid !== null && "email" in uid));
    }
    const userFinds = calls.find.filter((c) => c.collection === "users");
    assert.equal(userFinds.length, 1, "one batched users find for the page");
  });

  test("inside db.transaction(...) — tx.x.find(...).with(...) works", async () => {
    const { db, calls } = makeDb();
    const { data, error } = await db.transaction(async (tx) => {
      const rows = await tx.todos.find({ projectId: 10 }).with({ userId: true });
      return rows;
    });
    assert.equal(error, null);
    assert.ok(data);
    const t100 = data!.find((r) => r.id === 100) as AnyRec;
    assert.deepEqual(t100.userId, { id: 1, email: "alice@example.com", name: "Alice" });
    // The tx body's find against users still routes through the per-collection
    // surface (TX_CONN handles the connection routing in Rust); we only verify
    // the join happened.
    const userFinds = calls.find.filter((c) => c.collection === "users");
    assert.equal(userFinds.length, 1);
  });

  test("with: {} (empty spec) is a no-op — relation step never runs", async () => {
    const { db, calls } = makeDb();
    const { data } = await db.todos.find({ projectId: 10 }, { with: {} });
    assert.ok(data);
    // Original todos shape preserved; no users find fired.
    const userFinds = calls.find.filter((c) => c.collection === "users");
    assert.equal(userFinds.length, 0);
    const t100 = data!.find((r) => r.id === 100) as AnyRec;
    assert.equal(t100.userId, 1, "FK number is left in place when no relation requested");
  });

  test("with: { userId: false } at runtime — strict `true` only, rejects", async () => {
    const { db } = makeDb();
    // Cast: TS type forbids `false`, but a JS caller could send it.
    const { data, error } = await db.todos.find(
      {},
      { with: { userId: false } as unknown as { userId: true } },
    );
    assert.equal(data, null);
    assert.ok(error);
    assert.match(error!.message, /only `true` is supported/);
  });
});

describe("with: type-level inference (compile-time)", () => {
  test("Row<S> & WithRelations<S, W, AllSchemas> resolves the joined key to Row<TargetSchema>", async () => {
    const { db } = makeDb();
    const { data } = await db.todos.find({ projectId: 10 }, { with: { userId: true } });
    if (!data) return;
    const first = data[0];
    // After the v2 generics refactor `first.userId` is `Row<usersSchema> | null`
    // — no cast needed. `first.userId.email` is `string`, `first.userId.id`
    // is `number`, etc. This is the load-bearing assertion: it would NOT
    // compile under the v1 `WithRelations<W>` (which widened to PlainObject).
    if (first.userId !== null) {
      const email: string = first.userId.email;
      const name: string = first.userId.name;
      const id: number = first.userId.id;
      assert.equal(typeof email, "string");
      assert.equal(typeof name, "string");
      assert.equal(typeof id, "number");
    }
  });
});

// ---------------------------------------------------------------------------
// Parallelism: two relations load concurrently, not in series. The old
// `for..of await` loop in `_loadRelations` made N relations a 2× / 3× latency
// hit even though each relation hits a disjoint target table.
// ---------------------------------------------------------------------------

describe("with: parallel relation loading", () => {
  /** Build a mock that injects an artificial 50ms delay into every
   *  `find` against the named target tables. Allows us to assert that
   *  two relation loaders run concurrently (≈ 50ms, not 100ms). */
  function makeSlowMock(
    tables: Record<string, Record<number, AnyRec>>,
    slowTargets: Set<string>,
    delayMs: number,
  ): { native: ZeroshipDb; calls: CallLog } {
    const calls: CallLog = { find: [], findBatched: [], findSingle: [] };
    const native = {
      registerModel: () => Promise.resolve(),
      // P9 PR 3: native `transaction(callback)` orchestrator stub.
      transaction: async (cb: (raw: unknown) => unknown) => cb(undefined),
      collection(name: string) {
        return {
          async find(filter: AnyRec, opts: AnyRec) {
            calls.find.push({ collection: name, filter, opts });
            if (slowTargets.has(name)) {
              await new Promise((r) => setTimeout(r, delayMs));
            }
            const rows = tables[name] ?? {};
            const idClause = filter.id as { $in?: number[] } | number | undefined;
            if (
              idClause !== null &&
              typeof idClause === "object" &&
              Array.isArray((idClause as AnyRec).$in)
            ) {
              const ids = (idClause as { $in: number[] }).$in;
              return ids.map((i) => rows[i]).filter(Boolean);
            }
            const out: AnyRec[] = [];
            for (const r of Object.values(rows)) {
              let ok = true;
              for (const [k, v] of Object.entries(filter)) {
                if (k.startsWith("$")) continue;
                if (r[k] !== v) { ok = false; break; }
              }
              if (ok) out.push(r);
            }
            return out;
          },
          async insert(_doc: AnyRec) { return _doc; },
        };
      },
    };
    return { native: native as unknown as ZeroshipDb, calls };
  }

  test("two slow relations load in parallel, not sequentially", async () => {
    const tables: Record<string, Record<number, AnyRec>> = {
      users: { 1: { id: 1, name: "Alice" }, 2: { id: 2, name: "Bob" } },
      projects: { 10: { id: 10, name: "Apollo" }, 11: { id: 11, name: "Beacon" } },
      todos: {
        100: { id: 100, userId: 1, projectId: 10, title: "buy milk" },
        101: { id: 101, userId: 2, projectId: 11, title: "write tests" },
      },
    };
    const mock = makeSlowMock(tables, new Set(["users", "projects"]), 50);
    const db = installSchemaForTest(
      {
        users: { name: t.string().required() },
        projects: { name: t.string().required() },
        todos: {
          userId: t.ref("users"),
          projectId: t.ref("projects").required(),
          title: t.string().required(),
        },
      },
      { native: mock.native, naming: { toColumn: s => s, toField: s => s } },
    );

    const started = performance.now();
    const { data, error } = await db.todos.find({}, { with: { userId: true, projectId: true } });
    const elapsed = performance.now() - started;

    assert.equal(error, null);
    assert.ok(data);
    // Sanity check that the joins actually happened — otherwise a
    // timing assertion would pass for the wrong reason.
    const t100 = data!.find((r) => r.id === 100) as AnyRec;
    assert.deepEqual(t100.userId, { id: 1, name: "Alice" });
    assert.deepEqual(t100.projectId, { id: 10, name: "Apollo" });

    // Sequential: ≥ 100ms (50 + 50). Parallel: ~50ms. We give parallel
    // a generous 80ms ceiling to cover scheduler jitter on slow CI.
    assert.ok(
      elapsed < 80,
      `expected < 80ms for parallel relation loading, got ${elapsed.toFixed(1)}ms (sequential would be >= 100ms)`,
    );
  });
});

// ---------------------------------------------------------------------------
// Safety: Query.with() must throw if the Query was constructed without a
// relation loader (i.e. via `new Query(...)` directly, bypassing
// `Collection.find`). Old behaviour: silent no-op.
// ---------------------------------------------------------------------------

describe("Query.with — guards against direct Query construction", () => {
  test("calling .with() on a Query with no loader throws TypeError", () => {
    // Construct a Query directly, skipping the optional `loadRelations`
    // argument that `Collection.find` normally passes.
    const q = new Query<unknown, unknown>(
      "todos",
      {} as ZeroshipDbFilter,
      async () => [],
    );
    assert.throws(
      () => q.with({ userId: true }),
      (e: unknown) => {
        assert.ok(e instanceof TypeError);
        assert.match((e as Error).message, /Collection\.find/);
        assert.match((e as Error).message, /direct Query construction/);
        return true;
      },
    );
  });
});

// ---------------------------------------------------------------------------
// FK coercion: post-PR 3 the FK column type cascaded to TEXT typed_id,
// so a string FK value is the canonical shape (not an error). The loader
// still rejects values that are neither string, number, nor bigint — a
// JSON object or array can never round-trip as a row id.
// bigint values coerce via `.toString()` so 64-bit ids stay lossless.
// ---------------------------------------------------------------------------

describe("with: non-numeric FK coercion + loud failure", () => {
  test("string FK value joins successfully (typed_id wire shape)", async () => {
    // **P7 PR 3** — Pre-PR 3 this same test asserted a string FK threw
    // `with_fk_not_numeric`; PR 3 widened the contract so a string FK
    // is the canonical shape (the FK column type cascaded to TEXT). The
    // value "1" matches the `users[1]` row through the mock's
    // string-aware lookup.
    const tables: Record<string, Record<number, AnyRec>> = {
      users: { 1: { id: 1, name: "Alice" } },
      todos: {
        100: { id: 100, userId: "1" as unknown as number, title: "buy milk" },
      },
    };
    const mock = makeMock(tables);
    const db = installSchemaForTest(
      {
        users: { name: t.string().required() },
        todos: {
          userId: t.ref("users"),
          title: t.string().required(),
        },
      },
      { native: mock.native, naming: { toColumn: s => s, toField: s => s } },
    );

    const { data, error } = await db.todos.find({}, { with: { userId: true } });
    assert.equal(error, null);
    assert.ok(data);
    const t100 = data!.find((r) => r.id === 100) as AnyRec;
    assert.deepEqual(t100.userId, { id: 1, name: "Alice" });
  });

  test("non-id-shaped FK value (object) throws with_fk_not_id_shaped", async () => {
    // **P7 PR 3** — only string / number / bigint are valid id shapes;
    // an object / array / boolean FK value still throws because no
    // typed_id or numeric id can ever serialise as one of those.
    const tables: Record<string, Record<number, AnyRec>> = {
      users: { 1: { id: 1, name: "Alice" } },
      todos: {
        100: { id: 100, userId: { malformed: true } as unknown as number, title: "buy milk" },
      },
    };
    const mock = makeMock(tables);
    const db = installSchemaForTest(
      {
        users: { name: t.string().required() },
        todos: {
          userId: t.ref("users"),
          title: t.string().required(),
        },
      },
      { native: mock.native, naming: { toColumn: s => s, toField: s => s } },
    );

    const { data, error } = await db.todos.find({}, { with: { userId: true } });
    assert.equal(data, null, "object FK must surface as a typed error");
    assert.ok(error);
    assert.match(
      error!.message,
      /_loadRelations: FK value for field 'userId' is not a string \/ number \/ bigint \(got object\)/,
    );
  });

  test("bigint FK value coerces and joins successfully", async () => {
    const tables: Record<string, Record<number, AnyRec>> = {
      users: { 1: { id: 1, name: "Alice" } },
      todos: {
        100: { id: 100, userId: 1n as unknown as number, title: "buy milk" },
      },
    };
    const mock = makeMock(tables);
    const db = installSchemaForTest(
      {
        users: { name: t.string().required() },
        todos: {
          userId: t.ref("users"),
          title: t.string().required(),
        },
      },
      { native: mock.native, naming: { toColumn: s => s, toField: s => s } },
    );

    const { data, error } = await db.todos.find({}, { with: { userId: true } });
    assert.equal(error, null);
    assert.ok(data);
    const t100 = data!.find((r) => r.id === 100) as AnyRec;
    assert.deepEqual(t100.userId, { id: 1, name: "Alice" });
  });
});

// ---------------------------------------------------------------------------
// Soft-delete + relations: the documented contract is that a FK pointing at
// a soft-deleted target yields `null` (because the target's `_mergeFilter`
// hides soft-deleted rows from every read, including the relation loader's
// batched IN). Conflates "null FK", "missing target", and "soft-deleted
// target" — but is intentional. Lock the contract.
// ---------------------------------------------------------------------------

describe("with: soft-delete + relations contract", () => {
  test("FK pointing at a soft-deleted target yields null in the joined field", async () => {
    // Mock that honours the `deletedAt: null` filter clause for the
    // `$in` branch (the default mock skips this — we need it here).
    const tables: Record<string, Record<number, AnyRec>> = {
      users: {
        1: { id: 1, name: "Alice", deletedAt: null },
        2: { id: 2, name: "Bob", deletedAt: 1700000000000 }, // soft-deleted
      },
      todos: {
        100: { id: 100, userId: 1, title: "alice todo" },
        101: { id: 101, userId: 2, title: "bob todo" },
      },
    };
    const calls: CallLog = { find: [], findBatched: [], findSingle: [] };
    const native = {
      registerModel: () => Promise.resolve(),
      // P9 PR 3: native `transaction(callback)` orchestrator stub.
      transaction: async (cb: (raw: unknown) => unknown) => cb(undefined),
      collection(name: string) {
        return {
          async find(filter: AnyRec, opts: AnyRec) {
            calls.find.push({ collection: name, filter, opts });
            const rows = tables[name] ?? {};
            // _mergeFilter on a soft-delete collection wraps the user's
            // filter in `{ $and: [orig, { deletedAt: null }] }` (when the
            // original filter is non-empty) — flatten the top-level
            // `$and` so the id IN clause and the deletedAt clause are
            // both visible to the matcher.
            const clauses: AnyRec[] =
              Array.isArray(filter.$and)
                ? (filter.$and as AnyRec[])
                : [filter];
            let idIn: number[] | null = null;
            let wantDeletedAtNull = false;
            for (const c of clauses) {
              const idClause = c.id as { $in?: number[] } | number | undefined;
              if (
                idClause !== null &&
                typeof idClause === "object" &&
                Array.isArray((idClause as AnyRec).$in)
              ) {
                idIn = (idClause as { $in: number[] }).$in;
              }
              if ("deletedAt" in c && c.deletedAt === null) wantDeletedAtNull = true;
            }
            const matchSoftDelete = (r: AnyRec): boolean =>
              wantDeletedAtNull ? r.deletedAt === null : true;
            if (idIn !== null) {
              return idIn
                .map((i) => rows[i])
                .filter((r): r is AnyRec => Boolean(r) && matchSoftDelete(r));
            }
            // Whole-table scan honouring soft-delete only.
            const out: AnyRec[] = [];
            for (const r of Object.values(rows)) {
              if (!matchSoftDelete(r)) continue;
              out.push(r);
            }
            return out;
          },
          async insert(_doc: AnyRec) { return _doc; },
        };
      },
    };
    const db = installSchemaForTest(
      {
        users: schema({ name: t.string().required() }).softDelete(),
        todos: {
          userId: t.ref("users"),
          title: t.string().required(),
        },
      },
      { native: native as unknown as ZeroshipDb, naming: { toColumn: s => s, toField: s => s } },
    );

    const { data, error } = await db.todos.find({}, { with: { userId: true } });
    assert.equal(error, null);
    assert.ok(data);
    const t100 = data!.find((r) => r.id === 100) as AnyRec;
    const t101 = data!.find((r) => r.id === 101) as AnyRec;
    assert.deepEqual(t100.userId, { id: 1, name: "Alice", deletedAt: null });
    assert.equal(
      t101.userId,
      null,
      "FK to a soft-deleted user must yield null in the joined field",
    );
  });
});

// ---------------------------------------------------------------------------
// Self-referencing FK: `users.managerId: t.ref("users")` must work, including
// when a row is its own manager and when many rows share the same manager
// (dedup to one IN clause).
// ---------------------------------------------------------------------------

describe("with: self-referencing FK", () => {
  test("self-ref join works, including a user that manages itself, and dedupes", async () => {
    const tables: Record<string, Record<number, AnyRec>> = {
      users: {
        1: { id: 1, name: "Alice", managerId: null },
        2: { id: 2, name: "Bob", managerId: 1 },
        3: { id: 3, name: "Carol", managerId: 1 }, // shares manager with Bob → dedup
        5: { id: 5, name: "Eve", managerId: 5 },   // manages themselves
      },
    };
    const mock = makeMock(tables);
    const db = installSchemaForTest(
      {
        users: {
          name: t.string().required(),
          managerId: t.ref("users"),
        },
      },
      { native: mock.native, naming: { toColumn: s => s, toField: s => s } },
    );

    const { data, error } = await db.users.find({}, { with: { managerId: true } });
    assert.equal(error, null);
    assert.ok(data);
    const alice = data!.find((r) => r.id === 1) as AnyRec;
    const bob = data!.find((r) => r.id === 2) as AnyRec;
    const carol = data!.find((r) => r.id === 3) as AnyRec;
    const eve = data!.find((r) => r.id === 5) as AnyRec;

    assert.equal(alice.managerId, null, "null manager FK stays null");
    assert.deepEqual(bob.managerId, { id: 1, name: "Alice", managerId: null });
    assert.deepEqual(carol.managerId, { id: 1, name: "Alice", managerId: null });
    // Self-reference: Eve's manager is Eve. The joined row carries the
    // PRE-join shape — i.e. managerId: 5 (a number), not infinite-depth.
    assert.deepEqual(eve.managerId, { id: 5, name: "Eve", managerId: 5 });

    // The relation loader fired exactly ONE find against `users`
    // (the relation target). Dedup: distinct ids in the IN clause
    // should be [1, 5] (Alice + Eve), not [1, 1, 5].
    const userFinds = mock.calls.find.filter((c) => c.collection === "users");
    // First find is the outer `db.users.find({})`; the relation loader's
    // batched IN is the second.
    assert.equal(userFinds.length, 2);
    // **P7 PR 3** — typed_id string wire shape for FK ids.
    const idClause = userFinds[1].filter.id as { $in: string[] };
    const seen = new Set(idClause.$in);
    assert.equal(seen.size, idClause.$in.length, "ids must be deduped");
    assert.deepEqual([...seen].sort(), ["1", "5"]);
  });
});
