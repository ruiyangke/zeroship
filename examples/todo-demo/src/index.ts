"use server";

// Pure server functions, backed by @zeroship/db. No routing boilerplate —
// the bootstrap handles JSON-RPC dispatch for the "use server" exports.
//
// In dev, the vite-plugin boots a PGlite-backed Postgres automatically
// and sets DATABASE_URL, so this file works without any Postgres install.
// Data persists across restarts in `.zeroship/dev.db/`.

import { createDb, t } from "@zeroship/db";

// Declare the Todo model once at module scope. @zeroship/db dispatches
// registerModel() to the runtime on first use, which runs CREATE TABLE
// IF NOT EXISTS. `id` is implicit (the runtime adds SERIAL PRIMARY KEY).
const db = createDb({
  todos: {
    text: t.string().required(),
    done: t.boolean().default(false),
  },
});

export interface Todo {
  id: number;
  text: string;
  done: boolean;
}

export async function listTodos(): Promise<Todo[]> {
  const r = await db.todos.find().sort({ id: 1 });
  if (r.error) throw r.error;
  return r.data as Todo[];
}

export async function addTodo(text: string): Promise<Todo> {
  const r = await db.todos.create({ text });
  if (r.error) throw r.error;
  return r.data as Todo;
}

export async function toggleTodo(id: number): Promise<Todo | null> {
  const found = await db.todos.findById(id);
  if (found.error) throw found.error;
  if (!found.data) return null;
  const upd = await db.todos.findOneAndUpdate({ id }, { done: !found.data.done });
  if (upd.error) throw upd.error;
  return upd.data as Todo | null;
}

export async function deleteTodo(id: number): Promise<boolean> {
  const r = await db.todos.findOneAndDelete({ id });
  if (r.error) throw r.error;
  return r.data !== null;
}
