"use server";

// db-todos — real-world example exercising @zeroship/db v2 surfaces.
//
// What this file demonstrates:
//   • export default { schema } convention — multi-collection schema
//     declared once; the platform installs typed wrappers on env.db
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
import type { Db } from "@zeroship/db";
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
// Actions — can call fetch(); cannot directly write the DB (must use
// runMutation). Auto-tx is NOT applied here — actions are long-lived and
// shouldn't hold a database transaction open across HTTP calls.
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
