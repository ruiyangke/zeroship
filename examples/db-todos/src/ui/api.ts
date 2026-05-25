// Tiny typed client for the db-todos RPC surface.
//
// The platform wire contract is dead simple, so we hit it directly with
// `fetch` instead of pulling in the full rpc-client stack:
//   • call:   POST /_zs/v1/<procedure>   body { "json": <input> }
//   • result: 200 { "json": <result> }   error: 4xx/5xx { message, code }
//   • stream: GET  /_zs/v1/<procedure>?input=<base64url {"json":<input>}>
//             AI-SDK SSE frames — `2:[<json>]` per object, `d:{}` to end.

const RPC = "/_zs/v1";

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

/** Error carrying the platform's typed `code` (SCREAMING_SNAKE). */
export class RpcError extends Error {
  code: string;
  constructor(message: string, code = "ERROR") {
    super(message);
    this.name = "RpcError";
    this.code = code;
  }
}

async function call<T>(proc: string, input: unknown): Promise<T> {
  let res: Response;
  try {
    res = await fetch(`${RPC}/${proc}`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ json: input ?? null }),
    });
  } catch (e) {
    throw new RpcError(
      `network error calling ${proc}: ${(e as Error).message}`,
      "NETWORK",
    );
  }
  const text = await res.text();
  let body: unknown = null;
  try {
    body = text ? JSON.parse(text) : null;
  } catch {
    /* non-JSON body */
  }
  if (!res.ok) {
    const b = body as { message?: string; code?: string } | null;
    throw new RpcError(b?.message ?? `${proc} failed (${res.status})`, b?.code ?? `HTTP_${res.status}`);
  }
  return (body as { json?: T } | null)?.json as T;
}

// ── Queries ────────────────────────────────────────────────────────────
export const listTodos = (userId: string) =>
  call<Todo[]>("listTodos", { userId });

export const todoCount = (userId: string) =>
  call<number>("todoCount", { userId });

// ── Mutations ──────────────────────────────────────────────────────────
export const seedUser = (input: { email: string; name: string; handle: string }) =>
  call<User>("seedUser", input);

export const createTodo = (input: { userId: string; title: string; priority?: Priority }) =>
  call<Todo>("createTodo", input);

export const completeTodo = (id: string) => call<Todo>("completeTodo", { id });
export const archiveTodo = (id: string) => call<Todo>("archiveTodo", { id });
export const deleteTodo = (id: string) => call<Todo>("deleteTodo", { id });

// ── Realtime ─────────────────────────────────────────────────────────────
export interface ChangeEvent {
  kind?: string;
  collection?: string;
  op?: "insert" | "update" | "delete" | string;
  id?: string | number;
  [k: string]: unknown;
}

function b64url(s: string): string {
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/**
 * Open the `subscribeTodos` SSE stream. Returns a teardown fn.
 * Parses AI-SDK frames: `2:[<json>]` carries one (or more) change events.
 */
export function subscribeTodos(
  onEvent: (e: ChangeEvent) => void,
  onStatus?: (live: boolean) => void,
): () => void {
  const input = b64url(JSON.stringify({ json: {} }));
  const es = new EventSource(`${RPC}/subscribeTodos?input=${input}`);
  es.onopen = () => onStatus?.(true);
  es.onerror = () => onStatus?.(false);
  es.onmessage = (msg) => handleFrame(msg.data, onEvent);
  // The platform encodes frames as raw `2:[...]` lines, which some SSE
  // transports surface as the default (unnamed) event data above; if the
  // server uses explicit `event:` names this still no-ops safely.
  return () => {
    es.close();
    onStatus?.(false);
  };
}

function handleFrame(data: string, onEvent: (e: ChangeEvent) => void) {
  for (const line of data.split("\n")) {
    const trimmed = line.trimStart();
    if (!trimmed.startsWith("2:")) continue;
    try {
      const payload = JSON.parse(trimmed.slice(2));
      const events = Array.isArray(payload) ? payload : [payload];
      for (const e of events) if (e && typeof e === "object") onEvent(e as ChangeEvent);
    } catch {
      /* ignore malformed frame */
    }
  }
}
