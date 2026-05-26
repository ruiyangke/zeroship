"use server";
// Server entry — every WRAPPED export becomes an RPC method the client
// can call. The file-level `"use server"` directive at the top opts
// the file into the vite-plugin's RPC discovery; only exports wrapped
// in `procedure()` / `query()` / `mutation()` / `stream()` /
// `subscription()` from `@zeroship/rpc/server` become public endpoints.
// Plain helpers stay private to the server bundle.
//
// The `@zeroship/vite-plugin` transform replaces wrapped exports
// with HTTP-RPC stubs in the client bundle (calling `listTodos(input)`
// on the client just works), and dispatches them server-side via the
// synthetic SSR entry's `default.rpc(name, input, ctx)`.
//
// Validation: procedures opt into runtime input/output validation
// by passing `input` / `output` Zod schemas in the wrapper's second
// arg (or via the legacy `fn.config = { ... }` assignment). The
// synthetic SSR entry calls `.parse()` before invoking the handler;
// failures throw an INVALID_ARGUMENT envelope (status 400) to the wire.

import { query, stream } from "@zeroship/rpc/server";
import { z } from "@zeroship/server";

export interface Todo {
  id: number;
  text: string;
  done: boolean;
}

// Hardcoded — the demo is about the build pipeline, not persistence.
const TODOS: Todo[] = [
  { id: 1, text: "Read the .zship spec",   done: true  },
  { id: 2, text: "Build the CSR demo",      done: true  },
  { id: 3, text: "Verify the manifest",     done: false },
  { id: 4, text: "Stretch — wire up SSR",   done: false },
];

export const listTodos = query(
  async (input: { limit?: number }): Promise<Todo[]> => {
    const limit = input.limit ?? TODOS.length;
    return TODOS.slice(0, limit);
  },
  {
    id: "listTodos",
    // Reject invalid input at the wire boundary — the synthetic SSR
    // entry calls `.parse()` before the handler runs. Bad inputs (e.g.
    // a non-number `limit`) return 400 INVALID_ARGUMENT with the Zod
    // issues array in the body.
    input: z.object({
      limit: z.number().int().min(1).max(100).optional(),
    }),
    // Output validation runs in dev only (NODE_ENV !== "production").
    // Cheap correctness check; the production hot path skips it.
    output: z.array(
      z.object({
        id: z.number(),
        text: z.string(),
        done: z.boolean(),
      }),
    ),
  },
);

// Async-generator stream procedure.
//
// The `stream()` wrapper tags this as `kind: "stream"`. The synthetic
// SSR entry pipes the iterator into the AI-SDK Data Stream Protocol
// on the wire (`2:[<json>]\n` per yield, `d:{}\n` at end). Client
// code consumes via:
//
//   for await (const todo of rpc.searchTodos.stream({ query: "build" })) {
//     console.log(todo);
//   }
//
// Or hand `rpc.searchTodos.streamUrl({ query })` to ai-sdk's `useChat`.
export const searchTodos = stream(
  async function* (input: { query: string }): AsyncGenerator<Todo> {
    const q = input.query.toLowerCase();
    for (const todo of TODOS) {
      if (todo.text.toLowerCase().includes(q)) {
        yield todo;
        // Tiny await so the wire shows distinct chunks instead of one
        // microtask-fused buffer flush.
        await new Promise((r) => setTimeout(r, 30));
      }
    }
  },
  {
    id: "searchTodos",
    input: z.object({
      query: z.string().min(1).max(200),
    }),
    // Per-yield schema. The synthetic entry skips output validation for
    // streams (see rpc-registry.ts) — we still declare it for typing.
    output: z.object({
      id: z.number(),
      text: z.string(),
      done: z.boolean(),
    }),
  },
);
