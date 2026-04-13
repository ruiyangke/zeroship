/**
 * Todo App — minimal CRUD demo using @appbase/db
 *
 * Deploy:
 *   cd examples/todo-sdk && npm install
 *   appbase deploy . --app=<uuid> --control=http://localhost:9090 --key=<key>
 *
 * API (JSON-RPC):
 *   addTodo(text)         → { data: { id, text, done, createdAt } }
 *   getTodos()            → { data: [{ id, text, done, createdAt }, ...] }
 *   toggleTodo(id, done)  → { data: { matchedCount, modifiedCount } }
 *   deleteTodo(id)        → { data: { deletedCount } }
 *   getStats()            → { data: { total, done, remaining } }
 */

import { model } from "@appbase/db";

const todos = model("todos", {
  text: { type: String, required: true, minlength: 1 },
  done: { type: Boolean, default: false },
});

export async function addTodo(text: string) {
  return await todos.create({ text });
}

export async function getTodos() {
  return await todos.find({}).sort({ createdAt: -1 });
}

export async function toggleTodo(id: number, done: boolean) {
  return await todos.updateOne({ id }, { done });
}

export async function deleteTodo(id: number) {
  return await todos.deleteOne({ id });
}

export async function getStats() {
  const { data: total } = await todos.countDocuments({});
  const { data: done } = await todos.countDocuments({ done: true });
  return { data: { total, done, remaining: (total ?? 0) - (done ?? 0) }, error: null };
}
