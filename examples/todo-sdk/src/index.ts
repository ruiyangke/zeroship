/**
 * Todo App — minimal CRUD demo using @zeroship/db
 *
 * Deploy:
 *   cd examples/todo-sdk && npm install
 *   zeroship deploy . --app=<uuid> --control=http://localhost:9090 --key=<key>
 */

import { createDb } from "@zeroship/db";

const db = createDb({
  todos: {
    text: { type: String, required: true, minlength: 1 },
    done: { type: Boolean, default: false },
  },
});

export async function addTodo(text: string) {
  return await db.todos.create({ text });
}

export async function getTodos() {
  return await db.todos.find({}).sort({ createdAt: -1 });
}

export async function toggleTodo(id: number, done: boolean) {
  return await db.todos.updateOne({ id }, { done });
}

export async function deleteTodo(id: number) {
  return await db.todos.deleteOne({ id });
}

export async function getStats() {
  const { data: total } = await db.todos.countDocuments({});
  const { data: done } = await db.todos.countDocuments({ done: true });
  return { data: { total, done, remaining: (total ?? 0) - (done ?? 0) }, error: null };
}
