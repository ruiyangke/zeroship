"use server";

// Simple in-memory todo list — tests the Vite Environment API dev flow.

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

// HTTP handler for direct requests
export function onRequest(req: any): Response {
  const url = new URL(req.url);

  if (url.pathname === "/api/todos" && req.method === "GET") {
    return new Response(JSON.stringify(listTodos()), {
      headers: { "Content-Type": "application/json" },
    });
  }

  if (url.pathname === "/api/todos" && req.method === "POST") {
    const todo = addTodo("New todo " + Date.now());
    return new Response(JSON.stringify(todo), {
      status: 201,
      headers: { "Content-Type": "application/json" },
    });
  }

  if (url.pathname === "/api/health") {
    return new Response(JSON.stringify({ status: "ok", env: "zeroship-v8" }), {
      headers: { "Content-Type": "application/json" },
    });
  }

  return new Response(JSON.stringify({ error: "not found" }), {
    status: 404,
    headers: { "Content-Type": "application/json" },
  });
}
