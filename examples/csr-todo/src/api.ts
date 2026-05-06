// examples/csr-todo/src/api.ts
//
// Wire `@zeroship/rpc-client` directly against the spec wire
// (`/_zs/v1/<id>` with `{ json, meta? }` superjson envelope). The
// runtime's synthetic-entry serves both the new wire and the legacy
// `/_rpc/` shape; we use the new one and ship a hooks-on-function
// surface to React.

import {
  client,
  type ProcedureType,
  __makeProcedure,
  type ProcedureCaller,
} from "@zeroship/rpc-client";
import type { Todo } from "./server";

export type App = {
  listTodos: ProcedureType<"query", { limit?: number }, Todo[]>;
  searchTodos: ProcedureType<"stream", { query: string }, Todo>;
};

export const rpc = client<App>({
  baseUrl: "",
  transformer: "superjson",
});

// ── Hooks-On-Function Pattern ────────────────────────────────────────

const listTodosCaller: ProcedureCaller<{ limit?: number }, Todo[]> = (input) =>
  rpc.listTodos.query(input);

export const listTodos = __makeProcedure(listTodosCaller, {
  id: "listTodos",
  kind: "query",
}) as ((input: { limit?: number }) => Promise<Todo[]>) & {
  id: string;
  kind: "query";
  queryKey: (input?: { limit?: number }) => [string, ...unknown[]];
  useQuery: (
    input: { limit?: number },
    options?: Record<string, unknown>,
  ) => { data?: Todo[]; isLoading: boolean; isError: boolean; error: unknown };
  useSuspenseQuery: (
    input: { limit?: number },
    options?: Record<string, unknown>,
  ) => { data: Todo[] };
  invalidate: (input?: { limit?: number }) => Promise<void> | void;
  prefetch: (
    input: { limit?: number },
    options?: Record<string, unknown>,
  ) => Promise<unknown>;
};

const searchTodosCaller: ProcedureCaller<{ query: string }, Todo> = (input) =>
  rpc.searchTodos.stream(input);

export const searchTodos = __makeProcedure(searchTodosCaller, {
  id: "searchTodos",
  kind: "stream",
}) as ((input: { query: string }) => AsyncIterable<Todo>) & {
  id: string;
  kind: "stream";
  queryKey: (input?: { query: string }) => [string, ...unknown[]];
  useStream: (
    input: { query: string },
    options?: Record<string, unknown>,
  ) => {
    chunks: Todo[];
    isStreaming: boolean;
    error: Error | null;
    cancel: () => void;
  };
};
