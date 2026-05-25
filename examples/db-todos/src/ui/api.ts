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

type CreateInput = { userId: string; title: string; priority?: Priority };
type ById = { id: string };
type ByUser = { userId: string };

// The procedure map — mirrors the wrapped exports in src/index.ts.
export type App = {
  listTodos: ProcedureType<"query", ByUser, Todo[]>;
  todoCount: ProcedureType<"query", ByUser, number>;
  publicUser: ProcedureType<"mutation", Record<string, never>, User>;
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

/** Get-or-create the single shared "public ledger" user (everyone writes here). */
export const publicUser = () => rpc.publicUser.mutation({});
export const createTodo = (input: CreateInput) => rpc.createTodo.mutation(input);
export const completeTodo = (id: string) => rpc.completeTodo.mutation({ id });
export const archiveTodo = (id: string) => rpc.archiveTodo.mutation({ id });
export const deleteTodo = (id: string) => rpc.deleteTodo.mutation({ id });

function b64url(s: string): string {
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/**
 * Live change feed — async iterator over broker events.
 *
 * NOTE: we read the stream with a hand-rolled fetch reader rather than
 * `rpc.subscribeTodos.stream()`. The subscription GET is identical across
 * tabs (`?input=<base64 {json:{}}>`), and browsers COALESCE identical
 * in-flight streaming `fetch`es to one connection — so a second tab would
 * never get its own stream (only the first shows LIVE). A per-connection
 * `_n` nonce makes each request unique (mirroring how EventSource opens an
 * independent connection per instance), and we parse the AI-SDK Data Stream
 * frames (`2:[…]`) ourselves. `signal` cancels the read.
 */
export async function* subscribeTodos(signal?: AbortSignal): AsyncIterableIterator<ChangeEvent> {
  const input = b64url(JSON.stringify({ json: {} }));
  const nonce = `${Date.now().toString(36)}${Math.random().toString(36).slice(2, 8)}`;
  const res = await fetch(`/_zs/v1/subscribeTodos?input=${input}&_n=${nonce}`, {
    headers: { accept: "text/event-stream" },
    signal,
  });
  if (!res.ok || !res.body) throw new Error(`subscribeTodos stream failed (${res.status})`);

  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buf = "";
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) return;
      buf += decoder.decode(value, { stream: true });
      let nl: number;
      while ((nl = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, nl).trimStart();
        buf = buf.slice(nl + 1);
        if (!line.startsWith("2:")) continue; // AI-SDK object frame
        try {
          const payload = JSON.parse(line.slice(2));
          const events = Array.isArray(payload) ? payload : [payload];
          for (const e of events) if (e && typeof e === "object") yield e as ChangeEvent;
        } catch {
          /* skip malformed frame */
        }
      }
    }
  } finally {
    try {
      await reader.cancel();
    } catch {
      /* already torn down */
    }
  }
}
