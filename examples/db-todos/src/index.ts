"use server";

// db-todos — real-world example exercising @zeroship/db v2 surfaces.
//
// What this file demonstrates:
//   • multi-collection DB model with typed wrappers on env.db
//   • t.ref("users")  — typed cross-table relations + native FK
//   • .unique() / .index() — materialised as real backend indexes
//   • Input validation at the RPC boundary
//   • query() / mutation() / action() wrappers (B3) — capability-scoped:
//       - query   reads only
//       - mutation read+write
//       - action  can call fetch(); compose via runQuery/runMutation
//
// Wire IDs are explicit and dotted:
//   exports become RPC procedures at /__zeroship/v1/<id>.

import { env } from "zeroship";
import type { TransactionOptions } from "@zeroship/db";
import { query, mutation, action, stream } from "@zeroship/rpc/server";
import { runQuery } from "@zeroship/server";
import {
  parseCreateTodoInput,
  parseSeedUserInput,
  type CreateTodoInput,
  type SeedUserInput,
  type TodoSnapshot,
} from "./schema";

const db = env.db;

// Brand types flow from t.ref(): a todo's `userId` is `Id<"users">`.
// Passing a todo id where a user id is expected is a compile-time error.
type UserId = typeof db.users.Id;

export type { Priority, Todo, TodoSnapshot, User } from "./schema";

function typedIdFromWire<T extends string>(id: string, prefix: string): T {
  if (!id.startsWith(`${prefix}_`)) {
    throw Object.assign(new Error(`invalid ${prefix} id`), {
      code: "INVALID_ID",
    });
  }
  return id as T;
}

const userIdFromWire = (id: string): UserId => typedIdFromWire<UserId>(id, "user");

// ---------------------------------------------------------------------------
// Queries — read-only RPC procedures.
// ---------------------------------------------------------------------------

// Outside `db.transaction(...)` every Collection method returns
// `Result<T> = { data, error }`. These handlers unwrap so the RPC wire
// carries the bare value — throwing on error lets the platform's
// error envelope handle the rejection uniformly.

export const listTodos = query(
  async ({ userId }: { userId: string }) => {
    const { data, error } = await db.todos
      .find({ userId: userIdFromWire(userId), archived: false })
      .sort({ id: -1 });
    if (error) throw error;
    return data ?? [];
  },
  { id: "todos.list" },
);

// Paginated variant — returns { page, continueCursor, isDone }. Pass back
// the previous result's continueCursor to advance; pass null for the first
// page. The orderBy is bound to the cursor, so callers must pass the same
// sort across calls.
export const listTodosPage = query(
  async ({
    userId,
    cursor,
    numItems,
  }: {
    userId: string;
    cursor: string | null;
    numItems: number;
  }) => {
    const { data, error } = await db.todos
      .find({ userId: userIdFromWire(userId) })
      .sort({ id: 1 })
      .paginate({ cursor, numItems });
    if (error) throw error;
    return data;
  },
  { id: "todos.listPage" },
);

export const getTodo = query(
  async ({ id }: { id: string }) => {
    const { data, error } = await db.todos.get(id);
    if (error) throw error;
    return data;
  },
  { id: "todos.get" },
);

export const todoCount = query(
  async ({ userId }: { userId: string }) => {
    const { data, error } = await db.todos.count({ userId: userIdFromWire(userId) });
    if (error) throw error;
    return data ?? 0;
  },
  { id: "todos.count" },
);

// The independent tally the concurrency probes below cannot influence:
// how many rows really carry a given title, read through the ordinary
// (non-transaction) path after everything has settled. `todos.count` filters
// on `userId` only, which is a per-user total and useless for a probe that
// shares one user across many runs.
export const todoCountTitle = query(
  async ({ userId, title }: { userId: string; title: string }) => {
    const { data, error } = await db.todos.count({
      userId: userIdFromWire(userId),
      title,
    });
    if (error) throw error;
    return data ?? 0;
  },
  { id: "todos.countTitle" },
);

// Smoke for the per-collection DataLoader: two `db.users.get(id)` calls
// inside one `Promise.all([...])` MUST coalesce into a single underlying
// `WHERE id IN (...)` fetch. Returning both rows verifies the loader
// stitches results back to the right callers.
export const getUserPair = query(
  async ({ aId, bId }: { aId: string; bId: string }) => {
    const [a, b] = await Promise.all([db.users.get(aId), db.users.get(bId)]);
    if (a.error) throw a.error;
    if (b.error) throw b.error;
    return { a: a.data ?? null, b: b.data ?? null };
  },
  { id: "users.getPair" },
);

// Load the user alongside the foreign key.
export const listTodosWithUser = query(
  async ({ userId }: { userId: string }) => {
    const { data, error } = await db.todos.find(
      { userId: userIdFromWire(userId), archived: false },
      { with: { user: true } },
    );
    if (error) throw error;
    return data ?? [];
  },
  { id: "todos.listWithUser" },
);

// ---------------------------------------------------------------------------
// Subscriptions — long-lived streams; capability maps to action.
// The app-facing reactive primitive is `db.live(queryFn)`: it yields the
// initial query result and then fresh snapshots whenever watched tables
// change.
// ---------------------------------------------------------------------------

export const subscribeTodos = stream(
  async function* ({ userId }: { userId: string }) {
    const live = db.live(() =>
      db.todos
        .find({ userId: userIdFromWire(userId), archived: false })
        .sort({ id: -1 }),
    );
    try {
      for await (const rows of live) {
        const snapshot: TodoSnapshot = { kind: "snapshot", rows };
        yield snapshot;
      }
    } finally {
      live.close();
    }
  },
  { id: "todos.subscribe" },
);

// ---------------------------------------------------------------------------
// Mutations — read+write RPC procedures.
// ---------------------------------------------------------------------------

export const createTodo = mutation(
  async (input: CreateTodoInput) => {
    const args = parseCreateTodoInput(input);
    const { data, error } = await db.todos.insert({
      userId:   userIdFromWire(args.userId),
      title:    args.title,
      priority: args.priority ?? "medium",
      tags:     [],
      done:     false,
      archived: false,
    });
    if (error) throw error;
    return data;
  },
  { id: "todos.create" },
);

export const setTodoDone = mutation(
  async ({ id, done }: { id: string; done: boolean }) => {
    const { data, error } = await db.todos.update(id, { done });
    if (error) throw error;
    return data;
  },
  { id: "todos.setDone" },
);

export const archiveTodo = mutation(
  async ({ id }: { id: string }) => {
    const { data, error } = await db.todos.update(id, { archived: true });
    if (error) throw error;
    return data;
  },
  { id: "todos.archive" },
);

export const deleteTodo = mutation(
  async ({ id }: { id: string }) => {
    const { data, error } = await db.todos.delete(id);
    if (error) throw error;
    return data;
  },
  { id: "todos.delete" },
);

// ---------------------------------------------------------------------------
// Actions -- can call fetch(); cannot directly write the DB (must use
// runMutation).
//
// Neither action() nor mutation() opens a transaction implicitly.
// `mutation()` is `attach(handler, "mutation", config)` -- a capability TAG,
// nothing more; the dispatcher's only per-call frame is `__zsEnterKind` in
// native runtime dispatch, which sets a thread-local ProcedureKind; and
// plugin-db reads that kind in exactly one place, `refuse_if_query_capability`,
// which matches ONLY `ProcedureKind::Query` in order to refuse writes from a
// query(). Nothing anywhere opens a BEGIN per dispatch. The wrappers "do not
// open a transaction implicitly" and top-level `db.<table>.*` calls autocommit
// per operation.
//
// So a multi-step mutation() is NOT atomic unless it calls db.transaction()
// itself. Note that crates/zeroship-runtime/src/web/fetch/mod.rs:284 still tells
// callers the opposite ("Mutations are transactional and must complete
// quickly; holding a DB tx open across an outbound HTTP call would block
// other writers") when it refuses fetch() inside a mutation -- the refusal is
// right, the reason given for it is not.
// ---------------------------------------------------------------------------

type ShareInput = { id: string; webhookUrl: string };

export const shareToWebhook = action(
  async ({ id, webhookUrl }: ShareInput) => {
    // Read through a query so the call lands inside a read-only tx
    // (defense-in-depth — getTodo couldn't write anyway).
    const todo = await runQuery(getTodo, { id });
    if (!todo) {
      throw new Error("not_found");
    }
    const resp = await fetch(webhookUrl, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ todo }),
    });
    return { status: resp.status, ok: resp.ok };
  },
  { id: "todos.shareToWebhook" },
);

// ---------------------------------------------------------------------------
// Transactions -- `db.transaction(async tx => ...)`.
//
// Why these live HERE and not in `examples/db-e2e` (which has four real
// `db.transaction()` calls at src/server.ts:644/664/683/696): db-e2e declares
// its schema INLINE via `schema()`/`t.*`, has no `migrations/` and no
// `generated/zeroship/`, so its build emits a manifest with NO
// `runtime_descriptor`. Runtime boot then installs
// nothing on `env.db` and every handler hits `undefined.find` -- the #209
// mechanism db-todos itself hit before it was given migrations. Giving db-e2e
// a migration-first port is a much larger surface (vector/geo columns,
// masking, replication) with failure modes unrelated to transactions; db-todos
// is already proven end to end on BOTH tiers.
//
// Contract these exercise (crates/zeroship-data-orm/src/transaction/mod.rs):
//   - `transaction(fn)` returns `Result<R>` -- resolve -> commit, throw ->
//     rollback. There is no tx.commit()/tx.rollback().
//   - Collections handed to the callback THROW instead of returning Result.
//   - A `transaction()` opened while one is already active for this app emits
//     `SAVEPOINT zs_sp_<N>` on the SAME connection, so an inner failure rolls
//     back only to that savepoint (cap: MAX_SAVEPOINT_DEPTH = 8).
//   - Isolation is selected on the outermost transaction. SQLite accepts the
//     default or serializable; other levels are rejected before the callback.
//
// Every procedure returns counts read AFTER the transaction settles, through
// the ordinary (non-tx) path, so the harness can tell "the callback said it
// inserted" from "the row is actually there".
// ---------------------------------------------------------------------------

type TxInput = { userId: string; tag: string };

export const unmigratedTransactionProbe = mutation(
  async () => {
    const result = await db.transaction(async () => null);
    if (result.error) throw result.error;
    return result.data;
  },
  { id: "diagnostics.unmigratedTransaction" },
);

export const unmigratedStreamProbe = stream(
  async function* () {
    const result = await db.users.find({});
    if (result.error) throw result.error;
    yield result.data;
  },
  { id: "diagnostics.unmigratedStream" },
);

/** Flatten an error to the two fields that are comparable across tiers.
 *  Deliberately keeps `message` VERBATIM: a backend-specific string is
 *  exactly the kind of divergence this leg exists to surface. */
function errShape(e: unknown): { code: string | null; message: string } | null {
  if (e === null || e === undefined) return null;
  const o = e as { code?: unknown; message?: unknown };
  return {
    code: typeof o.code === "string" ? o.code : null,
    message: typeof o.message === "string" ? o.message : String(e),
  };
}

// Commit path + read-your-own-writes INSIDE the open transaction.
export const txCommit = mutation(
  async ({ userId, tag }: TxInput) => {
    const uid = userIdFromWire(userId);
    const result = await db.transaction(async (tx) => {
      const a = await tx.todos.insert({ userId: uid, title: `${tag}-a` });
      const b = await tx.todos.insert({ userId: uid, title: `${tag}-b`, priority: "high" });
      // Both rows must be visible to a read on the SAME connection before
      // COMMIT. A backend that routed this read to the pool instead of the
      // tx connection would answer 0 here and still commit.
      const seen = await tx.todos.find({ userId: uid, title: `${tag}-a` }).first();
      const inTxCount = await tx.todos.count({ userId: uid });
      return { aId: a.id, bTitle: b.title, bPriority: b.priority, seenTitle: seen?.title ?? null, inTxCount };
    });
    const after = await db.todos.count({ userId: uid });
    return {
      error: errShape(result.error),
      data: result.data ?? null,
      committedCount: after.data ?? null,
    };
  },
  { id: "todos.txCommit" },
);

// Rollback path: the callback throws AFTER a successful insert.
export const txRollback = mutation(
  async ({ userId, tag }: TxInput) => {
    const uid = userIdFromWire(userId);
    const result = await db.transaction(async (tx) => {
      await tx.todos.insert({ userId: uid, title: `${tag}-doomed` });
      const inTxCount = await tx.todos.count({ userId: uid, title: `${tag}-doomed` });
      throw Object.assign(new Error("probe rollback"), {
        code: "PROBE_ROLLBACK",
        inTxCount,
      });
    });
    const after = await db.todos.count({ userId: uid, title: `${tag}-doomed` });
    const visible = await db.todos.find({ userId: uid, title: `${tag}-doomed` });
    return {
      error: errShape(result.error),
      // Does the creator's own thrown error reach the caller verbatim, with
      // the extra property it was decorated with? That is the "what does the
      // caller receive" half of the contract.
      inTxCount: (result.error as { inTxCount?: unknown } | null)?.inTxCount ?? null,
      data: result.data ?? null,
      countAfter: after.data ?? null,
      visibleAfter: (visible.data ?? []).length,
    };
  },
  { id: "todos.txRollback" },
);

// Nested transaction = SAVEPOINT. The inner throw must roll back ONLY the
// inner insert; the outer transaction keeps going and commits its own row.
export const txNested = mutation(
  async ({ userId, tag }: TxInput) => {
    const uid = userIdFromWire(userId);
    const outer = await db.transaction(async (tx) => {
      await tx.todos.insert({ userId: uid, title: `${tag}-outer` });
      // NOTE the `db.` here, not `tx.` -- nesting is detected from the
      // isolate's tx slot, not from which handle you call.
      const inner = await db.transaction(async (tx2) => {
        await tx2.todos.insert({ userId: uid, title: `${tag}-inner` });
        throw Object.assign(new Error("probe inner abort"), { code: "PROBE_INNER" });
      });
      // Still inside the OUTER transaction, after ROLLBACK TO SAVEPOINT.
      const outerSeen = await tx.todos.count({ userId: uid, title: `${tag}-outer` });
      const innerSeen = await tx.todos.count({ userId: uid, title: `${tag}-inner` });
      return { innerError: errShape(inner.error), outerSeen, innerSeen };
    });
    const outerAfter = await db.todos.count({ userId: uid, title: `${tag}-outer` });
    const innerAfter = await db.todos.count({ userId: uid, title: `${tag}-inner` });
    return {
      error: errShape(outer.error),
      data: outer.data ?? null,
      outerAfter: outerAfter.data ?? null,
      innerAfter: innerAfter.data ?? null,
    };
  },
  { id: "todos.txNested" },
);

// The documented isolation-level divergence, and the invalid-level contract.
// `level: null` means "no opts at all" (plain BEGIN on both tiers).
export const txIsolation = mutation(
  async ({ userId, tag, level }: { userId: string; tag: string; level: string | null }) => {
    const uid = userIdFromWire(userId);
    const title = `${tag}-${level ?? "none"}`;
    // `threw` distinguishes a SYNCHRONOUS TypeError out of the native method
    // from a rejected Result. Measured rather than assumed: an unknown
    // isolationLevel is raised by normalize_isolation_level() before any SQL
    // runs, and it is not obvious from the source alone which of the two
    // paths the caller ends up on.
    let threw: ReturnType<typeof errShape> = null;
    let error: ReturnType<typeof errShape> = null;
    let data: unknown = null;
    try {
      const result = await db.transaction(
        async (tx) => {
          const r = await tx.todos.insert({ userId: uid, title });
          return { title: r.title };
        },
        // The probe also sends invalid input to exercise runtime rejection.
        level === null
          ? undefined
          : { isolationLevel: level as TransactionOptions["isolationLevel"] },
      );
      error = errShape(result.error);
      data = result.data ?? null;
    } catch (e) {
      threw = errShape(e);
    }
    const after = await db.todos.count({ userId: uid, title });
    return { threw, error, data, countAfter: after.data ?? null };
  },
  { id: "todos.txIsolation" },
);

// Savepoint depth. MAX_SAVEPOINT_DEPTH is 8, so `levels` = 1 outer BEGIN plus
// (levels - 1) SAVEPOINTs: 9 is the deepest that may open, 10 must be refused
// with `savepoint_depth_exceeded` BEFORE any SQL runs.
type TxDepthResult = {
  deepestLevel: number;
  refusedAtLevel: number | null;
  error: ReturnType<typeof errShape>;
};

export const txDepth = mutation(
  async ({ userId, tag, levels }: { userId: string; tag: string; levels: number }) => {
    const uid = userIdFromWire(userId);
    const title = `${tag}-d${levels}`;
    const nest = async (remaining: number, level: number): Promise<TxDepthResult> => {
      const r = await db.transaction(async (tx) => {
        if (remaining <= 1) {
          await tx.todos.insert({ userId: uid, title });
          return { deepestLevel: level, refusedAtLevel: null, error: null };
        }
        return await nest(remaining - 1, level + 1);
      });
      if (r.error) {
        return { deepestLevel: level - 1, refusedAtLevel: level, error: errShape(r.error) };
      }
      return r.data as TxDepthResult;
    };
    const out = await nest(levels, 1);
    // A refusal at ANY level must leave nothing behind: the deepest insert
    // never ran, and every enclosing savepoint is released rather than
    // committed only halfway.
    const after = await db.todos.count({ userId: uid, title });
    return { ...out, countAfter: after.data ?? null };
  },
  { id: "todos.txDepth" },
);

// ---------------------------------------------------------------------------
// CONCURRENT transactions -- the regime an isolation level exists for.
//
// Every probe above is a SINGLE uncontended transaction, so
// none of them can see what happens when two transactions for the same app are
// open at once. That is the whole point of `isolationLevel`.
//
// The mechanism under test (crates/zeroship-data-v8/src/context.rs:146 and
// crates/zeroship-data-orm/src/transaction/mod.rs:227):
//
//   * the open tx connection lives in `tx_conns: HashMap<app_id, TxConnection>`
//     -- ONE slot per app, per isolate.
//   * `transaction_dispatch` decides BEGIN-vs-SAVEPOINT **synchronously**, from
//     `has_tx_for(app_id)`, but the client is only installed LATER, inside the
//     async begin op.
//
// So there are two distinct windows, and these probes separate them:
//
//   txParallel  both `transaction()` calls happen in the SAME JS turn, so both
//               read `has_tx_for == false` and both take the top-level BEGIN
//               path. The second `install_tx_client` then overwrites the
//               first's slot.
//   txOverlap   the second call happens AFTER the first's BEGIN landed, so it
//               reads `has_tx_for == true` and nests as a SAVEPOINT on the
//               first transaction's connection -- even though the two are
//               logically unrelated units of work.
//
// Both are reachable from ONE request (`Promise.all`), so they do not depend on
// how the worker schedules requests across isolates. `txRaceStep` is the
// cross-REQUEST version and does.
// ---------------------------------------------------------------------------

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(() => r(), ms));

/** Two `db.transaction()` calls opened in the same JS turn.
 *
 *  Contract a creator would assume: two independent units of work, two rows,
 *  no errors. What is actually under test is whether the second BEGIN silently
 *  evicts the first from the per-app slot. */
export const txParallel = mutation(
  async ({ userId, tag }: TxInput) => {
    const uid = userIdFromWire(userId);
    const title = `${tag}-par`;
    const leg = async (n: number) => {
      const r = await db.transaction(async (tx) => {
        // `before` is read on whichever connection the slot holds at that
        // moment. Two legs on two real transactions both see 0; two legs
        // sharing one connection see 0 then 1.
        const before = await tx.todos.count({ userId: uid, title });
        await tx.todos.insert({ userId: uid, title });
        const after = await tx.todos.count({ userId: uid, title });
        return { before, after };
      });
      return { n, error: errShape(r.error), seen: r.data ?? null };
    };
    let threw: ReturnType<typeof errShape> = null;
    let legs: unknown = null;
    try {
      legs = await Promise.all([leg(1), leg(2)]);
    } catch (e) {
      threw = errShape(e);
    }
    const after = await db.todos.count({ userId: uid, title });
    return { threw, legs, countAfter: after.data ?? null };
  },
  { id: "todos.txParallel" },
);

/** Leg A opens a transaction, holds it open across an await, then ABORTS.
 *  Leg B opens its own transaction inside that window and COMMITS.
 *
 *  A creator's expectation: A's rollback undoes A's row and nothing else, so
 *  `aAfter == 0` and `bAfter == 1`. If B was silently folded into A's
 *  transaction as a savepoint, A's ROLLBACK also destroys B's committed-looking
 *  write and `bAfter == 0` while B reported success -- two unrelated units of
 *  work entangled. That is the outcome this probe exists to detect. */
export const txOverlap = mutation(
  async ({ userId, tag, holdMs }: TxInput & { holdMs: number }) => {
    const uid = userIdFromWire(userId);
    const titleA = `${tag}-oa`;
    const titleB = `${tag}-ob`;
    const legA = async () => {
      const r = await db.transaction(async (tx) => {
        await tx.todos.insert({ userId: uid, title: titleA });
        await sleep(holdMs);
        throw Object.assign(new Error("probe A aborts"), { code: "PROBE_A_ABORT" });
      });
      return { error: errShape(r.error) };
    };
    const legB = async () => {
      // Start inside A's open window: long enough that A's BEGIN has landed,
      // short enough that A has not yet thrown.
      await sleep(Math.max(1, Math.floor(holdMs / 2)));
      const r = await db.transaction(async (tx) => {
        await tx.todos.insert({ userId: uid, title: titleB });
        return { committed: true };
      });
      return { error: errShape(r.error), data: r.data ?? null };
    };
    let threw: ReturnType<typeof errShape> = null;
    let a: unknown = null;
    let b: unknown = null;
    try {
      [a, b] = await Promise.all([legA(), legB()]);
    } catch (e) {
      threw = errShape(e);
    }
    const aAfter = await db.todos.count({ userId: uid, title: titleA });
    const bAfter = await db.todos.count({ userId: uid, title: titleB });
    return { threw, a, b, aAfter: aAfter.data ?? null, bAfter: bAfter.data ?? null };
  },
  { id: "todos.txOverlap" },
);

/** An ORDINARY write that merely overlaps someone else's transaction.
 *
 *  `txOverlap` above asks what happens to a second `db.transaction()`.
 *  This asks the sharper question: does a plain `db.todos.insert()` — no
 *  transaction anywhere in its call chain — get pulled into a transaction
 *  that happens to be open for the same app?
 *
 *  It matters more than the transaction-vs-transaction case because it is
 *  the DEFAULT path: most creator writes are not inside a transaction, so
 *  if routing is ambient then every ordinary write issued while any one
 *  request holds a transaction open inherits that transaction's fate.
 *  Contract: `bAfter == 1` -- B's write is its own unit of work and A's
 *  rollback has no business touching it. */
export const txPlainWrite = mutation(
  async ({ userId, tag, holdMs }: TxInput & { holdMs: number }) => {
    const uid = userIdFromWire(userId);
    const titleA = `${tag}-pa`;
    const titleB = `${tag}-pb`;
    const legA = async () => {
      const r = await db.transaction(async (tx) => {
        await tx.todos.insert({ userId: uid, title: titleA });
        await sleep(holdMs);
        throw Object.assign(new Error("probe A aborts"), { code: "PROBE_A_ABORT" });
      });
      return { error: errShape(r.error) };
    };
    const legB = async () => {
      await sleep(Math.max(1, Math.floor(holdMs / 2)));
      const r = await db.todos.insert({ userId: uid, title: titleB });
      return { error: errShape(r.error), inserted: r.data !== null && r.data !== undefined };
    };
    let threw: ReturnType<typeof errShape> = null;
    let a: unknown = null;
    let b: unknown = null;
    try {
      [a, b] = await Promise.all([legA(), legB()]);
    } catch (e) {
      threw = errShape(e);
    }
    const aAfter = await db.todos.count({ userId: uid, title: titleA });
    const bAfter = await db.todos.count({ userId: uid, title: titleB });
    return { threw, a, b, aAfter: aAfter.data ?? null, bAfter: bAfter.data ?? null };
  },
  { id: "todos.txPlainWrite" },
);

/** Do writes on PARALLEL BRANCHES inside one transaction callback still
 *  belong to that transaction?
 *
 *  Every other tx probe awaits its writes in a straight line, so all of them
 *  would still pass if the platform decided "in a transaction" from ambient
 *  state. This one branches: `Promise.all` inside the callback creates two
 *  continuations that fork off the callback frame, and the routing decision is
 *  now taken per dispatch from the async context (`crates/plugin-db/src/
 *  tx_route.rs`). If a branch did NOT inherit the callback's scope it would be
 *  routed to the pool, autocommit on its own, and SURVIVE the abort below.
 *
 *  Contract: the callback throws, so `countAfter == 0` -- both branch writes
 *  are rolled back with everything else. `countAfter == 2` means the branches
 *  escaped the transaction; `1` means only one did. */
export const txBranchWrites = mutation(
  async ({ userId, tag }: TxInput) => {
    const uid = userIdFromWire(userId);
    const title = `${tag}-br`;
    const r = await db.transaction(async (tx) => {
      // Two writes with no `await` between them: both promises are created
      // in the callback frame and settle on separate continuations.
      await Promise.all([
        tx.todos.insert({ userId: uid, title }),
        tx.todos.insert({ userId: uid, title }),
      ]);
      const seen = await tx.todos.count({ userId: uid, title });
      throw Object.assign(new Error("branch probe aborts"), {
        code: "PROBE_BRANCH_ABORT",
        seen,
      });
    });
    const after = await db.todos.count({ userId: uid, title });
    return { error: errShape(r.error), countAfter: after.data ?? null };
  },
  { id: "todos.txBranchWrites" },
);

/** A write issued from a continuation that OUTLIVED its transaction.
 *
 *  The callback starts a promise and never awaits it; the transaction commits
 *  and releases its connection; the promise then wakes up and issues a write
 *  that was dispatched inside the (now dead) transaction scope.
 *
 *  There is no right connection for that write. The one outcome that must NOT
 *  happen is a silent success: work the creator wrote inside a transaction
 *  committing on its own after the transaction is gone. Contract: an error,
 *  and `countAfter == 0`. Reported verbatim so the ANSWER is measured rather
 *  than asserted into shape. */
export const txOrphanedWrite = mutation(
  async ({ userId, tag, holdMs }: TxInput & { holdMs: number }) => {
    const uid = userIdFromWire(userId);
    const titleTx = `${tag}-oa`;
    const titleOrphan = `${tag}-oo`;
    // Held in an array, not a `let`: TypeScript's control-flow analysis does
    // not see an assignment made inside a callback, so a `let orphan = null`
    // would still read as `null` at the await below.
    const orphans: Promise<{ data?: unknown; error?: unknown }>[] = [];
    const r = await db.transaction(async (tx) => {
      await tx.todos.insert({ userId: uid, title: titleTx });
      // Deliberately NOT awaited: it resolves after the callback returns and
      // after the COMMIT has released the tx connection.
      orphans.push(
        (async () => {
          await sleep(holdMs);
          return await db.todos.insert({ userId: uid, title: titleOrphan });
        })(),
      );
      return { committed: true };
    });
    let orphanError: ReturnType<typeof errShape> = null;
    let orphanThrew: ReturnType<typeof errShape> = null;
    let orphanInserted = false;
    const orphanStarted = orphans.length;
    try {
      const o = orphans.length > 0 ? await orphans[0] : null;
      orphanError = errShape(o?.error ?? null);
      orphanInserted = o?.data !== null && o?.data !== undefined;
    } catch (e) {
      orphanThrew = errShape(e);
    }
    const txAfter = await db.todos.count({ userId: uid, title: titleTx });
    const orphanAfter = await db.todos.count({ userId: uid, title: titleOrphan });
    return {
      error: errShape(r.error),
      orphanStarted,
      orphanError,
      orphanThrew,
      orphanInserted,
      txAfter: txAfter.data ?? null,
      orphanAfter: orphanAfter.data ?? null,
    };
  },
  { id: "todos.txOrphanedWrite" },
);

/** ONE half of a cross-REQUEST write-write race. The harness fires two of
 *  these at the same `tag` concurrently.
 *
 *  `t0`/`t1` are wall-clock at handler entry/exit, so the harness can tell
 *  whether the two dispatches actually OVERLAPPED before it reads anything into
 *  the answers. If `t0(second) >= t1(first)` the platform serialised them and
 *  no race was staged -- a refutation, not a pass.
 *
 *  `before` is the count read INSIDE the transaction. Two serialised
 *  transactions read 0 then 1; two truly concurrent ones both read 0, which is
 *  the lost-update signature `serializable` is supposed to reject. */
export const txRaceStep = mutation(
  async ({
    userId,
    tag,
    holdMs,
    level,
  }: TxInput & { holdMs: number; level: string | null }) => {
    const uid = userIdFromWire(userId);
    const t0 = Date.now();
    let threw: ReturnType<typeof errShape> = null;
    let error: ReturnType<typeof errShape> = null;
    let data: unknown = null;
    try {
      const r = await db.transaction(
        async (tx) => {
          const before = await tx.todos.count({ userId: uid, title: tag });
          // The hold is what makes the window wide enough for the other
          // request to land inside it.
          await sleep(holdMs);
          const row = await tx.todos.insert({
            userId: uid,
            title: tag,
            priority: before === 0 ? "low" : "high",
          });
          return { before, priority: row.priority };
        },
        level === null
          ? undefined
          : { isolationLevel: level as TransactionOptions["isolationLevel"] },
      );
      error = errShape(r.error);
      data = r.data ?? null;
    } catch (e) {
      threw = errShape(e);
    }
    const t1 = Date.now();
    const after = await db.todos.count({ userId: uid, title: tag });
    return { t0, t1, threw, error, data, countAfter: after.data ?? null };
  },
  { id: "todos.txRaceStep" },
);

export const seedUser = mutation(
  async (input: SeedUserInput) => {
    const { email, name, handle } = parseSeedUserInput(input);
    const { data, error } = await db.users.insert({ email, name, handle });
    if (error) throw error;
    return data;
  },
  { id: "users.seed" },
);

// Shared "public ledger" user — every client writes into ONE list so the
// realtime feed streams to all viewers (no per-window identity). Get-or-
// create on a fixed unique handle; `todos.userId` is a required FK, so we
// keep a single stable user rather than dropping the relation.
const PUBLIC_HANDLE = "ledger";

export const publicUser = mutation(
  async (_input: Record<string, never>) => {
    const found = await db.users.find({ handle: PUBLIC_HANDLE });
    if (found.error) throw found.error;
    if (found.data && found.data.length > 0) return found.data[0];

    const created = await db.users.insert({
      handle: PUBLIC_HANDLE,
      email: "ledger@public.local",
      name: "Public Ledger",
    });
    if (created.error) {
      // Lost a create race against another window — return the existing row.
      const retry = await db.users.find({ handle: PUBLIC_HANDLE });
      if (retry.data && retry.data.length > 0) return retry.data[0];
      throw created.error;
    }
    return created.data;
  },
  { id: "users.public" },
);
