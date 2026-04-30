// examples/csr-todo/src/api.ts
//
// Wire the `@zeroship/rpc-client` against the legacy `/_rpc/<modPath>/<exportName>`
// endpoint that the current runtime exposes. The client's spec wire is
// `/_zs/v1/<id>` with a `{ json, meta }` superjson envelope; we install
// a custom `fetch` wrapper that transparently rewrites both directions
// for this demo so the same `rpc.<id>.query()` surface works against
// today's runtime.
//
// When Phase 2's gateway lands and the runtime starts answering
// `/_zs/v1/<id>` with the spec wire, the wrapper here disappears and
// the demo just calls `client({ baseUrl })` directly.

import {
  client,
  type ProcedureType,
  __makeProcedure,
  type ProcedureCaller,
} from "@zeroship/rpc-client";
import type { Todo } from "./server";

// ── App type — describes the procedures the server exposes ───────────
//
// Phase 3 picked the `procedures` registry approach (Option B-fallback
// per the spec). Each entry pins the kind + input/output types. This
// keeps the surface fully type-safe end-to-end without virtual modules
// or a tooling-emitted .d.ts file.
//
// Phase 4 — `searchTodos` is a stream procedure. The handle exposes
// `.stream(input): AsyncIterableIterator<Todo>` plus `.streamUrl(input)`
// for handing to ai-sdk's `useChat`.
export type App = {
  listTodos: ProcedureType<"query", { limit?: number }, Todo[]>;
  searchTodos: ProcedureType<"stream", { query: string }, Todo>;
};

/**
 * Custom fetch that adapts spec-shape requests to the legacy /_rpc
 * endpoint:
 *
 *   GET  /_zs/v1/listTodos?input=<base64url-superjson>
 *     →  POST /_rpc/src/server/listTodos with body = [<input>]
 *
 *   POST /_zs/v1/<id>           (mutation, or fallback query)
 *     →  POST /_rpc/src/server/<id>  with body unwrapped to positional
 *
 * Response: the legacy runtime returns a plain JSON value (no
 * `{json, meta}` envelope). We re-wrap into `{ json: ... }` so the
 * client's superjson decode still finds the right shape.
 */
const adapterFetch: typeof globalThis.fetch = async (input, init) => {
  const url = new URL(typeof input === "string" ? input : input.toString());
  const v1Match = url.pathname.match(/^\/_zs\/v1\/(.+)$/);
  if (!v1Match) {
    return globalThis.fetch(input, init);
  }
  const id = v1Match[1];
  // The csr-todo example exports its server module as `src/server.ts`,
  // so the legacy method name is `src/server/<exportName>`.
  const legacyName = `src/server/${id}`;
  const legacyUrl = `${url.origin}/_rpc/${legacyName}`;

  // Decode the input — either from the query string (small GET) or the
  // request body (POST / fallback POST).
  let inputValue: unknown;
  if (init?.method === "POST" && typeof init.body === "string") {
    // Body is `{ json, meta? }` (superjson envelope).
    try {
      const parsed = JSON.parse(init.body);
      inputValue = parsed.json ?? parsed;
    } catch {
      inputValue = undefined;
    }
  } else {
    const inputParam = url.searchParams.get("input");
    if (inputParam) {
      // base64url → JSON.
      const b64 = inputParam.replace(/-/g, "+").replace(/_/g, "/");
      const padded = b64 + "=".repeat((4 - (b64.length % 4)) % 4);
      const decoded = atob(padded);
      try {
        const parsed = JSON.parse(decoded);
        inputValue = parsed.json ?? parsed;
      } catch {
        inputValue = undefined;
      }
    }
  }

  // Build the legacy request — body is a positional-args JSON array.
  const args = inputValue === undefined ? [] : [inputValue];
  const headers = new Headers(init?.headers);
  headers.set("Content-Type", "application/json");
  headers.delete("X-Method"); // legacy server doesn't understand the fallback header.

  const res = await globalThis.fetch(legacyUrl, {
    method: "POST",
    headers,
    body: JSON.stringify(args),
    signal: init?.signal,
  });

  // Re-wrap the response body so the client's superjson decoder sees
  // the expected `{ json, meta? }` shape. Errors surface through the
  // status code; the client's parseErrorResponse handles them.
  if (!res.ok) return res;
  // Pass SSE / event-stream responses through verbatim — they're the
  // AI-SDK Data Stream wire (`<typeId>:<json>\n` per line) and the
  // client's `streamCall` parser expects raw bytes, not a JSON envelope.
  const ct = res.headers.get("Content-Type") ?? "";
  if (ct.includes("text/event-stream")) {
    return res;
  }
  const text = await res.text();
  let value: unknown;
  try {
    value = text ? JSON.parse(text) : null;
  } catch {
    value = text;
  }
  return new Response(JSON.stringify({ json: value }), {
    status: 200,
    headers: { "Content-Type": "application/json" },
  });
};

export const rpc = client<App>({
  // Same-origin — Vite's dev server proxies /_rpc to the runtime; the
  // production build serves both / and /_rpc from the same gateway.
  baseUrl: "",
  fetch: adapterFetch,
  // The legacy /_rpc endpoint speaks plain JSON; once the spec wire
  // lands, switch to "superjson".
  transformer: "superjson",
});

// ── Phase 5 — hooks-on-function pattern (§10) ───────────────────────
//
// Every procedure-shaped export is BOTH a callable and a hooks object:
//
//   await listTodos({ limit: 50 })            // direct call
//   listTodos.useQuery({ limit: 50 })         // React Query hook
//   listTodos.invalidate()                    // cache invalidation
//
// Once the vite-plugin's client transform emits `__makeProcedure(...)`
// per server export, this lives in the auto-generated stub file. For
// the demo we wrap manually — the SAME `rpc.listTodos.query(...)`
// transport drives the underlying call.

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
