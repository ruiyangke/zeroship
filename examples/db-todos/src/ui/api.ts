// Typed client for the db-todos RPC surface, built on @zeroship/rpc-client.
// Each procedure is wrapped with `__makeProcedure` so it's both directly
// callable AND carries React Query hooks (`.useQuery` / `.useMutation` /
// `.setData` / `.invalidate` / `.queryKey`) once `@zeroship/rpc-react` is
// imported at the app root. Mirrors examples/csr-todo.

import {
  client,
  __makeProcedure,
  RpcError,
  type ProcedureType,
  type ProcedureCaller,
} from "@zeroship/rpc-client";

export { RpcError };

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

type CreateInput = { userId: string; title: string; priority?: Priority };
type ById = { id: string };
type ByUser = { userId: string };

export type App = {
  listTodos: ProcedureType<"query", ByUser, Todo[]>;
  publicUser: ProcedureType<"mutation", Record<string, never>, User>;
  createTodo: ProcedureType<"mutation", CreateInput, Todo>;
  completeTodo: ProcedureType<"mutation", ById, Todo>;
  archiveTodo: ProcedureType<"mutation", ById, Todo>;
  deleteTodo: ProcedureType<"mutation", ById, Todo>;
};

const rpc = client<App>({ baseUrl: "", transformer: "superjson" });

// `__makeProcedure` returns a loose `ProcedureFn` union; these precise shapes
// expose exactly the hooks-on-function surface we use (the methods exist at
// runtime once @zeroship/rpc-react populates the registry).
type UseQueryResult<O> = { data?: O; isLoading: boolean; isError: boolean; error: unknown };
type UseMutationResult<I, O> = {
  mutate: (input: I) => void;
  mutateAsync: (input: I) => Promise<O>;
  isPending: boolean;
};
export type QueryProc<I, O> = ((i: I) => Promise<O>) & {
  useQuery: (i: I, opts?: Record<string, unknown>) => UseQueryResult<O>;
  queryKey: (i?: I) => unknown[];
  setData: (i: I, updater: O | ((old: O | undefined) => O)) => void;
  invalidate: (i?: I) => Promise<void>;
};
export type MutationProc<I, O> = ((i: I) => Promise<O>) & {
  useMutation: (opts?: Record<string, unknown>) => UseMutationResult<I, O>;
};

const q = <I, O>(id: keyof App, fn: (i: I) => Promise<O>) =>
  __makeProcedure(fn as ProcedureCaller<I, O>, { id: id as string, kind: "query" }) as unknown as QueryProc<I, O>;
const m = <I, O>(id: keyof App, fn: (i: I) => Promise<O>) =>
  __makeProcedure(fn as ProcedureCaller<I, O>, { id: id as string, kind: "mutation" }) as unknown as MutationProc<I, O>;

// Hooks-on-function procedures.
export const listTodos = q<ByUser, Todo[]>("listTodos", (i) => rpc.listTodos.query(i));
export const publicUser = m<Record<string, never>, User>("publicUser", (i) => rpc.publicUser.mutation(i));
export const createTodo = m<CreateInput, Todo>("createTodo", (i) => rpc.createTodo.mutation(i));
export const completeTodo = m<ById, Todo>("completeTodo", (i) => rpc.completeTodo.mutation(i));
export const archiveTodo = m<ById, Todo>("archiveTodo", (i) => rpc.archiveTodo.mutation(i));
export const deleteTodo = m<ById, Todo>("deleteTodo", (i) => rpc.deleteTodo.mutation(i));

// ── Realtime ─────────────────────────────────────────────────────────────
function b64url(s: string): string {
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/**
 * Live change feed — async iterator over broker events. We read the stream
 * with a hand-rolled fetch reader (not `rpc...stream()`): the subscription
 * GET is identical across tabs (`?input=<base64 {json:{}}>`), and browsers
 * COALESCE identical in-flight streaming fetches to one connection — a 2nd
 * tab would never get its own stream. A per-connection `_n` nonce makes each
 * request unique (like EventSource's independent connections); we parse the
 * AI-SDK Data Stream frames (`2:[…]`) ourselves. `signal` cancels.
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
