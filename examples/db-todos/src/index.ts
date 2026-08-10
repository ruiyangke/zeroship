"use server";

// db-todos — real-world example exercising @zeroship/db v2 surfaces.
//
// What this file demonstrates:
//   • multi-collection DB model with typed wrappers on env.db
//   • t.ref("users")  — typed cross-table relations + native FK
//   • .unique() / .index() — materialised as real backend indexes
//   • Validation: required / min / max / enum / pattern (existing SDK)
//   • query() / mutation() / action() wrappers (B3) — capability-scoped:
//       - query   reads only
//       - mutation read+write
//       - action  can call fetch(); compose via runQuery/runMutation
//
// Wire IDs are explicit and dotted:
//   exports become RPC procedures at /__zeroship/v1/<id>.

import { env } from "zeroship";
import type { Db, TransactionOptions } from "@zeroship/db";
import { query, mutation, action, stream } from "@zeroship/rpc/server";
import { runQuery } from "@zeroship/server";
import {
  dbSchema,
  type Priority,
  type SeedUserInput,
  type Todo,
  type TodoSnapshot,
  type User,
} from "./schema";

export default { schema: dbSchema };

// Local `db` shorthand for this legacy inline-schema example. New apps get
// this module augmentation from generated/zeroship/env.db.ts.
const db = env.db as Db<typeof dbSchema>;

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

// Smoke for relation-aware reads: `find({}, { with: { userId: true } })`
// must attach the joined user row at each todo's `userId` field in a
// single batched roundtrip across all rows.
export const listTodosWithUser = query(
  async ({ userId }: { userId: string }) => {
    const { data, error } = await db.todos.find(
      { userId: userIdFromWire(userId), archived: false },
      { with: { userId: true } },
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

type CreateTodoInput = {
  userId: string;
  title: string;
  priority?: Priority;
};

export const createTodo = mutation(
  async (args: CreateTodoInput) => {
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
// An earlier version of this comment said "Auto-tx is NOT applied here",
// implying mutation() gets an implicit transaction and action() does not.
// It does not, and neither does mutation(). Verified 2026-08-10:
// `mutation()` is `attach(handler, "mutation", config)` (sdks/rpc/src/
// server.ts:225) -- a capability TAG, nothing more; the dispatcher's only
// per-call frame is `__zsEnterKind` (sdks/bootstrap/src/dispatcher.ts:140-146),
// which sets a thread-local ProcedureKind; and plugin-db reads that kind in
// exactly one place, `refuse_if_query_capability` (v8_bridge.rs:82), which
// matches ONLY `ProcedureKind::Query` in order to refuse writes from a
// query(). Nothing anywhere opens a BEGIN per dispatch.
// docs/reference/db.md:898-902 states the same: the wrappers "do not open a
// transaction implicitly" and top-level `db.<table>.*` calls autocommit per
// operation.
//
// So a multi-step mutation() is NOT atomic unless it calls db.transaction()
// itself. Note that crates/runtime/src/web/fetch/mod.rs:284 still tells
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
// Added 2026-08-10 for the transaction third of scenario 3. Until then this
// example had ZERO transaction calls (both greps hit COMMENTS, above), so the
// dev-vs-deployed harness could not compare commit / rollback / savepoint
// behaviour on the two backends at all.
//
// Why these live HERE and not in `examples/db-e2e` (which has four real
// `db.transaction()` calls at src/server.ts:644/664/683/696): db-e2e declares
// its schema INLINE via `schema()`/`t.*`, has no `migrations/` and no
// `generated/zeroship/`, so its build emits a manifest with NO
// `runtime_descriptor` (measured 2026-08-10). Runtime boot then installs
// nothing on `env.db` and every handler hits `undefined.find` -- the #209
// mechanism db-todos itself hit before it was given migrations. Giving db-e2e
// a migration-first port is a much larger surface (vector/FTS/geo columns,
// masking, replication) with failure modes unrelated to transactions; db-todos
// is already proven end to end on BOTH tiers.
//
// Contract these exercise (crates/plugin-db/src/transaction/mod.rs):
//   - `transaction(fn)` returns `Result<R>` -- resolve -> commit, throw ->
//     rollback. There is no tx.commit()/tx.rollback().
//   - Collections handed to the callback THROW instead of returning Result.
//   - A `transaction()` opened while one is already active for this app emits
//     `SAVEPOINT zs_sp_<N>` on the SAME connection, so an inner failure rolls
//     back only to that savepoint (cap: MAX_SAVEPOINT_DEPTH = 8).
//   - `{ isolationLevel }` is honoured on the outermost BEGIN only. Postgres
//     emits `BEGIN ISOLATION LEVEL ...`; SQLite validates the string and then
//     runs a plain `BEGIN` (transaction/mod.rs:384-391). That divergence is
//     documented in docs/reference/sqlite-divergences.md and had never been
//     measured; `txIsolation` below is the probe that measures whether it is
//     observable through this surface at all.
//
// Every procedure returns counts read AFTER the transaction settles, through
// the ordinary (non-tx) path, so the harness can tell "the callback said it
// inserted" from "the row is actually there".
// ---------------------------------------------------------------------------

type TxInput = { userId: string; tag: string };

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
        // The cast is NOT only there for `txIsoBad`. THREE spellings-of-record
        // exist for this option and no two agree (measured 2026-08-10):
        //
        //   the TypeScript union   sdks/types/shared.d.ts:18-22 allows ONLY the
        //                          SQL-spaced forms: "read uncommitted" |
        //                          "read committed" | "repeatable read" |
        //                          "serializable".
        //   the reference doc      docs/reference/db.md:893-894 tells creators
        //                          to write "readCommitted" (default),
        //                          "repeatableRead" or "serializable".
        //   the runtime            normalize_isolation_level
        //                          (crates/plugin-db/src/v8_classes/db.rs:295)
        //                          accepts camelCase, spaced and uppercase, all
        //                          four levels -- and its rejection message
        //                          recommends the camelCase spellings.
        //
        // So the two spellings the reference doc recommends do not compile.
        // Verified with `tsc --noEmit` on this project: `{ isolationLevel:
        // "repeatableRead" }` gives `TS2820: Type '"repeatableRead"' is not
        // assignable to type 'ZeroshipIsolationLevel | undefined'. Did you mean
        // '"repeatable read"'?`, and the same for "readCommitted", while
        // "repeatable read" and "serializable" compile clean. They WORK at
        // runtime: this probe sends "repeatableRead" and Postgres was observed
        // emitting `BEGIN ISOLATION LEVEL REPEATABLE READ` for it.
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

export const seedUser = mutation(
  async ({ email, name, handle }: SeedUserInput) => {
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
