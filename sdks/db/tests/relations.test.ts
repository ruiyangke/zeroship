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
import { createDb } from "../src/db.js";
import { t } from "../src/types.js";

type AnyRec = Record<string, unknown>;

type CallLog = {
  find: { collection: string; filter: AnyRec; opts: AnyRec }[];
  findOne: { collection: string; filter: AnyRec; opts: AnyRec }[];
};

/** Mock native that returns rows by table. The find handler understands
 *  `{ id: { $in: [...] } }` so the relation loader's batched IN works. */
function makeMock(
  tables: Record<string, Record<number, AnyRec>>,
): { native: ZeroshipDb; calls: CallLog } {
  const calls: CallLog = { find: [], findOne: [] };
  const native = {
    registerModel: () => Promise.resolve(),
    beginTransaction: async () => ({
      commit: async () => undefined,
      rollback: async () => undefined,
    }),
    collection(name: string) {
      return {
        async findOne(filter: AnyRec, opts: AnyRec) {
          calls.findOne.push({ collection: name, filter, opts });
          const rows = tables[name] ?? {};
          const id = filter.id;
          if (typeof id === "number") return rows[id] ?? null;
          for (const r of Object.values(rows)) {
            let ok = true;
            for (const [k, v] of Object.entries(filter)) {
              if (r[k] !== v) { ok = false; break; }
            }
            if (ok) return r;
          }
          return null;
        },
        async find(filter: AnyRec, opts: AnyRec) {
          calls.find.push({ collection: name, filter, opts });
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
          if (typeof idClause === "number") {
            const r = rows[idClause];
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
    calls.findOne = mock.calls.findOne;
  }
  const db = createDb(
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
    const idClause = userFinds[0].filter.id as { $in: number[] };
    assert.deepEqual([...idClause.$in].sort((a, b) => a - b), [1, 2]);
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

    // get(id) with `with` skips the DataLoader path and goes through findOne.
    assert.equal(calls.findOne.length, 1, "get(id, {with}) bypasses the loader");
    assert.equal(calls.findOne[0].collection, "todos");
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
    const idClause = userFinds[0].filter.id as { $in: number[] };
    // Only the distinct non-null user ids should appear: 1 and 2 (104's 9999 too).
    const seen = new Set(idClause.$in);
    assert.equal(seen.size, idClause.$in.length, "ids must be deduped");
    assert.ok(seen.has(1));
    assert.ok(seen.has(2));
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
  test("Row<S> & WithRelations<W> exposes the joined key at the type layer", async () => {
    const { db } = makeDb();
    const { data } = await db.todos.find({ projectId: 10 }, { with: { userId: true } });
    if (!data) return;
    const first = data[0];
    // `userId` widens to `PlainObject | null` — narrowing on null is a real
    // runtime check; the rest is type-only assertion via assignability.
    if (first.userId !== null) {
      // The joined value is the target row; we know it has an `id` field.
      const _id: unknown = (first.userId as Record<string, unknown>).id;
      assert.ok(_id !== undefined);
    }
  });
});
