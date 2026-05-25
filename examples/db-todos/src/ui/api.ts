// Direct backend calls — the supported path. We import the wrapped server
// procedures straight from the "use server" module (src/index.ts); the
// @zeroship/vite-plugin transform rewrites these client-side imports into
// typed RPC callers over /_zs/v1/<id>. No hand-rolled client.
//
// `query()`/`mutation()` return `typeof handler`, and branded ids
// (UserId/TodoId) are just strings over the wire — so we re-type the
// callers with plain client-facing shapes here.
import {
  listTodos as _listTodos,
  createTodo as _createTodo,
  completeTodo as _completeTodo,
  archiveTodo as _archiveTodo,
  deleteTodo as _deleteTodo,
  publicUser as _publicUser,
} from "../index";

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
  /** Client-only: stable render key bridging an optimistic row to its real id. */
  _key?: string;
}

export interface ChangeEvent {
  kind?: string;
  op?: "insert" | "update" | "delete" | string;
  collection?: string;
  pk?: string | number;
  columns?: string[];
}

type ByUser = { userId: string };
type ById = { id: string };
type CreateInput = { userId: string; title: string; priority?: Priority };

export const listTodos = _listTodos as unknown as (i: ByUser) => Promise<Todo[]>;
export const createTodo = _createTodo as unknown as (i: CreateInput) => Promise<Todo>;
export const completeTodo = _completeTodo as unknown as (i: ById) => Promise<Todo>;
export const archiveTodo = _archiveTodo as unknown as (i: ById) => Promise<Todo>;
export const deleteTodo = _deleteTodo as unknown as (i: ById) => Promise<Todo>;
export const publicUser = _publicUser as unknown as (i: Record<string, never>) => Promise<User>;

/** Error shape thrown by the transform's RPC stubs (plain Error + .code). */
export const errCode = (e: unknown): string | undefined =>
  (e as { code?: string } | null)?.code;

// ── Realtime ─────────────────────────────────────────────────────────────
function b64url(s: string): string {
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/**
 * Live change feed — async iterator over broker events. Hand-rolled fetch
 * reader (not a generated stub) because the subscription GET is identical
 * across tabs and browsers COALESCE identical streaming fetches; a per-
 * connection `_n` nonce gives each tab its own stream. Parses the AI-SDK
 * Data Stream frames (`2:[…]`). `signal` cancels.
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
        if (!line.startsWith("2:")) continue;
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
