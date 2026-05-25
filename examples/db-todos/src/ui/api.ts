// Typed client for the db-todos RPC surface, built on the platform's
// `@zeroship/rpc-client` — the same wire the vite-plugin discovers on the
// server side (`"use server"` exports in src/index.ts). We declare the
// procedure map (`App`) and the client hands back per-procedure handles:
//   • rpc.listTodos.query(input)        → Promise<result>
//   • rpc.createTodo.mutation(input)    → Promise<result>
//   • rpc.subscribeTodos.stream(input)  → AsyncIterableIterator (AI-SDK
//                                          Data Stream frames, parsed for us)
//
// This is why the realtime feed needs the client and not a raw EventSource:
// the platform streams the AI-SDK Data Stream Protocol (`2:[…]` frames),
// which EventSource (SSE `data:` only) cannot parse — `.stream()` can.

import { client, type ProcedureType } from "@zeroship/rpc-client";

export type Priority = "low" | "medium" | "high";

export interface User {
  id: string;
  created_at: number;
  updated_at: number;
  version: number;
  email: string;
  name: string;
  handle: string;
}

export interface Todo {
  id: string;
  created_at: number;
  updated_at: number;
  version: number;
  userId: string;
  title: string;
  priority: Priority;
  tags: string[];
  done: boolean;
  archived: boolean;
  deleted_at: number | null;
}

/** Change event from the broker (`subscribe("todos")`). */
export interface ChangeEvent {
  kind?: string;
  op?: "insert" | "update" | "delete" | string;
  collection?: string;
  pk?: string | number;
  columns?: string[];
}

type SeedInput = { email: string; name: string; handle: string };
type CreateInput = { userId: string; title: string; priority?: Priority };
type ById = { id: string };
type ByUser = { userId: string };

// The procedure map — mirrors the wrapped exports in src/index.ts.
export type App = {
  listTodos: ProcedureType<"query", ByUser, Todo[]>;
  todoCount: ProcedureType<"query", ByUser, number>;
  getUserPair: ProcedureType<"query", { aId: string; bId: string }, { a: User | null; b: User | null }>;
  seedUser: ProcedureType<"mutation", SeedInput, User>;
  createTodo: ProcedureType<"mutation", CreateInput, Todo>;
  completeTodo: ProcedureType<"mutation", ById, Todo>;
  archiveTodo: ProcedureType<"mutation", ById, Todo>;
  deleteTodo: ProcedureType<"mutation", ById, Todo>;
  subscribeTodos: ProcedureType<"stream", Record<string, never>, ChangeEvent>;
};

export const rpc = client<App>({ baseUrl: "", transformer: "superjson" });

// Re-export the structured error so the UI can branch on `.code`.
export { RpcError } from "@zeroship/rpc-client";

// ── thin wrappers (keep the component import surface tidy) ──────────────
export const listTodos = (userId: string) => rpc.listTodos.query({ userId });
export const todoCount = (userId: string) => rpc.todoCount.query({ userId });

/** Existence probe: a stored session id is stale if the row is gone (dev DB reset). */
export const userExists = async (id: string): Promise<boolean> => {
  try {
    const { a } = await rpc.getUserPair.query({ aId: id, bId: id });
    return a != null;
  } catch {
    return false;
  }
};
export const seedUser = (input: SeedInput) => rpc.seedUser.mutation(input);
export const createTodo = (input: CreateInput) => rpc.createTodo.mutation(input);
export const completeTodo = (id: string) => rpc.completeTodo.mutation({ id });
export const archiveTodo = (id: string) => rpc.archiveTodo.mutation({ id });
export const deleteTodo = (id: string) => rpc.deleteTodo.mutation({ id });

/** Live change feed — async iterator over broker events. `signal` cancels. */
export const subscribeTodos = (signal?: AbortSignal): AsyncIterableIterator<ChangeEvent> =>
  rpc.subscribeTodos.stream({}, signal ? { signal } : undefined);
