// Server entry — every export becomes an RPC method the client can call.
//
// File location is the marker: `src/server.ts` (or anywhere under
// `src/server/**`) is server-only by convention. The `@zeroship/vite-plugin`
// transform replaces these exports with HTTP-RPC stubs in the client
// bundle, and registers them under `<modulePath>/<exportName>` on the
// server (so the wire path is `POST /_rpc/src/server/listTodos`).
// Calling `listTodos()` from client code Just Works.
//
// Validation: procedures opt into runtime input/output validation
// by setting `fn.config.input` / `fn.config.output` to a Zod schema.
// The synthetic SSR entry calls `.parse()` before invoking the
// handler; failures throw an INVALID_ARGUMENT envelope (status 400)
// to the wire.

import { z } from "@zeroship/server";

export interface Todo {
  id: number;
  text: string;
  done: boolean;
}

// Hardcoded — the demo is about the build pipeline, not persistence.
const TODOS: Todo[] = [
  { id: 1, text: "Read the .zsapp spec",   done: true  },
  { id: 2, text: "Build the CSR demo",      done: true  },
  { id: 3, text: "Verify the manifest",     done: false },
  { id: 4, text: "Stretch — wire up SSR",   done: false },
];

export async function listTodos(input: { limit?: number } = {}): Promise<Todo[]> {
  const limit = input.limit ?? TODOS.length;
  return TODOS.slice(0, limit);
}
listTodos.config = {
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
};
