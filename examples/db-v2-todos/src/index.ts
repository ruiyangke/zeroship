"use server";

// db-v2-todos — real-world example exercising @zeroship/db v2 surfaces.
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

import { createDb, t, schema, type Id } from "@zeroship/db";
import { query, mutation, action } from "@zeroship/server";

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

// Brand types flow from t.ref(): Id<"users"> and Id<"todos"> are
// incompatible — passing a post id where a user id is expected is a
// type error.
type UserId = Id<"users">;
type TodoId = Id<"todos">;

// ---------------------------------------------------------------------------
// Queries — read-only; auto-wrapped in BEGIN TRANSACTION READ ONLY (T1)
// ---------------------------------------------------------------------------

export const listTodos = query(
  async ({ userId }: { userId: UserId }, ctx) => {
    const col = (ctx.db as any).todos;
    return col.find({ userId, archived: false }).sort({ createdAt: -1 });
  },
);

export const getTodo = query(
  async ({ id }: { id: TodoId }, ctx) => {
    const col = (ctx.db as any).todos;
    return col.findOne({ id });
  },
);

export const todoCount = query(
  async ({ userId }: { userId: UserId }, ctx) => {
    const col = (ctx.db as any).todos;
    return col.countDocuments({ userId });
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
  async (args: CreateTodoInput, ctx) => {
    const col = (ctx.db as any).todos;
    return col.create({
      userId:   args.userId,
      title:    args.title,
      priority: args.priority ?? "medium",
      tags:     [],
      done:     false,
      archived: false,
    });
  },
);

export const completeTodo = mutation(
  async ({ id }: { id: TodoId }, ctx) => {
    const col = (ctx.db as any).todos;
    return col.updateOne({ id }, { done: true });
  },
);

export const archiveTodo = mutation(
  async ({ id }: { id: TodoId }, ctx) => {
    const col = (ctx.db as any).todos;
    return col.updateOne({ id }, { archived: true });
  },
);

export const deleteTodo = mutation(
  async ({ id }: { id: TodoId }, ctx) => {
    const col = (ctx.db as any).todos;
    return col.deleteOne({ id });
  },
);

// ---------------------------------------------------------------------------
// Actions — can call fetch(); cannot directly write the DB (must use
// runMutation). Auto-tx is NOT applied here — actions are long-lived and
// shouldn't hold a Postgres transaction open across HTTP calls.
// ---------------------------------------------------------------------------

type ShareInput = { id: TodoId; webhookUrl: string };

export const shareToWebhook = action(
  async ({ id, webhookUrl }: ShareInput, ctx) => {
    // Read through a query — actions can't access ctx.db.* directly.
    const todo = await ctx.runQuery(getTodo as any, { id });
    if (!todo) {
      throw new Error("not_found");
    }
    const resp = await ctx.fetch(webhookUrl, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ todo }),
    });
    return { status: resp.status, ok: resp.ok };
  },
);

// ---------------------------------------------------------------------------
// Re-export the migration so the deploy can register it
// ---------------------------------------------------------------------------

export { backfillArchived } from "./migrations.js";
