import type { Priority, Todo } from "./types";

export const TODO_PRIORITIES: readonly Priority[] = ["low", "medium", "high"];

export function formatTodoAge(createdAtMs: number, nowMs = Date.now()): string {
  const s = Math.max(0, Math.round((nowMs - createdAtMs) / 1000));
  if (s < 5) return "now";
  if (s < 60) return `${s}s`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.round(m / 60);
  if (h < 24) return `${h}h`;
  return `${Math.round(h / 24)}d`;
}

export function isPendingTodo(todo: Pick<Todo, "id">): boolean {
  return todo.id.startsWith("tmp_");
}

export function partitionTodos(todos: readonly Todo[]): { active: Todo[]; done: Todo[] } {
  const active: Todo[] = [];
  const done: Todo[] = [];
  for (const todo of todos) {
    if (todo.done) done.push(todo);
    else active.push(todo);
  }
  return { active, done };
}
