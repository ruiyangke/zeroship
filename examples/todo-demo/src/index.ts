"use server";

// Pure server functions — no routing boilerplate needed.
// The bootstrap handles JSON-RPC dispatch automatically.

interface Todo {
  id: number;
  text: string;
  done: boolean;
}

let nextId = 1;
const todos: Todo[] = [];

export function listTodos(): Todo[] {
  return todos;
}

export function addTodo(text: string): Todo {
  const todo: Todo = { id: nextId++, text, done: false };
  todos.push(todo);
  return todo;
}

export function toggleTodo(id: number): Todo | null {
  const todo = todos.find((t) => t.id === id);
  if (!todo) return null;
  todo.done = !todo.done;
  return todo;
}

export function deleteTodo(id: number): boolean {
  const idx = todos.findIndex((t) => t.id === id);
  if (idx === -1) return false;
  todos.splice(idx, 1);
  return true;
}
