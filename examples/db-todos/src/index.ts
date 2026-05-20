"use server";

// db-todos — real-world example exercising @zeroship/db v2 surfaces.
//
// What this file demonstrates:
//   • createDb({...}) with multi-collection schema
//   • t.ref("users")  — typed cross-table relations + Postgres FK (B2)
//   • .unique() / .index() — materialised as real Postgres indexes (A1)
//   • Validation: required / min / max / enum / pattern (existing SDK)
//   • query() / mutation() / action() wrappers (B3) — capability-scoped:
//       - query   reads only; runs inside READ ONLY tx
//       - mutation read+write; runs inside SERIALIZABLE tx; refuses fetch
//       - action  can call fetch(); no auto-tx; must use runQuery/runMutation
//   • Migration audit log (A3) — backfillArchived in ./migrations.ts
//
// Wire conventions match the existing examples (hono-demo etc.):
//   exports become RPC procedures at /_zs/v1/<name>.

import { createDb, t, schema } from "@zeroship/db";
import { query, mutation, action, runQuery } from "@zeroship/server";

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

export const db = createDb({
  users: {
    email:  t.string().required().unique(),
    name:   t.string().required().max(100),
    handle: t.string().required().unique().pattern(/^[a-z0-9_]+$/),
  },

  todos: schema({
    userId:   t.ref("users").required(),
    title:    t.string().required().min(1).max(200),
    priority: t.string().enum("low", "medium", "high").default("medium"),
    tags:     t.array(t.string()),
    done:     t.boolean().default(false),
    // `archived` field — added in a later deploy and backfilled via
    // the migration in ./migrations.ts. Documents the expand-migrate-
    // contract pattern: ship the field as nullable, run the migration,
    // then tighten if needed.
    archived: t.boolean().default(false),
  }),
});

// Brand types flow from t.ref(): `typeof db.users.Id` is `Id<"users">`
// and `typeof db.todos.Id` is `Id<"todos">` — passing a post id where
// a user id is expected is a compile-time error.
type UserId = typeof db.users.Id;
type TodoId = typeof db.todos.Id;

// ---------------------------------------------------------------------------
// Queries — read-only; auto-wrapped in BEGIN TRANSACTION READ ONLY (T1)
// ---------------------------------------------------------------------------

// Outside `db.transaction(...)` every Collection method returns
// `Result<T> = { data, error }`. These handlers unwrap so the RPC wire
// carries the bare value — throwing on error lets the platform's
// error envelope handle the rejection uniformly.

export const listTodos = query(
  async ({ userId }: { userId: UserId }) => {
    const { data, error } = await db.todos
      .find({ userId, archived: false })
      .sort({ createdAt: -1 });
    if (error) throw error;
    return data ?? [];
  },
);

export const getTodo = query(
  async ({ id }: { id: TodoId }) => {
    const { data, error } = await db.todos.get(id);
    if (error) throw error;
    return data;
  },
);

export const todoCount = query(
  async ({ userId }: { userId: UserId }) => {
    const { data, error } = await db.todos.count({ userId });
    if (error) throw error;
    return data ?? 0;
  },
);

// ---------------------------------------------------------------------------
// Mutations — read+write; auto-wrapped in BEGIN ISOLATION LEVEL SERIALIZABLE
// ---------------------------------------------------------------------------

type CreateTodoInput = {
  userId: UserId;
  title: string;
  priority?: "low" | "medium" | "high";
};

export const createTodo = mutation(
  async (args: CreateTodoInput) => {
    const { data, error } = await db.todos.insert({
      userId:   args.userId,
      title:    args.title,
      priority: args.priority ?? "medium",
      tags:     [],
      done:     false,
      archived: false,
    });
    if (error) throw error;
    return data;
  },
);

export const completeTodo = mutation(
  async ({ id }: { id: TodoId }) => {
    const { data, error } = await db.todos.update(id, { done: true });
    if (error) throw error;
    return data;
  },
);

export const archiveTodo = mutation(
  async ({ id }: { id: TodoId }) => {
    const { data, error } = await db.todos.update(id, { archived: true });
    if (error) throw error;
    return data;
  },
);

export const deleteTodo = mutation(
  async ({ id }: { id: TodoId }) => {
    const { data, error } = await db.todos.delete(id);
    if (error) throw error;
    return data;
  },
);

// ---------------------------------------------------------------------------
// Actions — can call fetch(); cannot directly write the DB (must use
// runMutation). Auto-tx is NOT applied here — actions are long-lived and
// shouldn't hold a Postgres transaction open across HTTP calls.
// ---------------------------------------------------------------------------

type ShareInput = { id: TodoId; webhookUrl: string };

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
);

// Seed helper — used by smoke.sh to provision users.
type SeedUserInput = { email: string; name: string; handle: string };

export const seedUser = mutation(
  async ({ email, name, handle }: SeedUserInput) => {
    const { data, error } = await db.users.insert({ email, name, handle });
    if (error) throw error;
    return data;
  },
);

// ---------------------------------------------------------------------------
// Re-export the migration so the deploy can register it
// ---------------------------------------------------------------------------

export { backfillArchived } from "./migrations.js";
