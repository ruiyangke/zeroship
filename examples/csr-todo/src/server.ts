"use server";
// Server entry — every export becomes an RPC method the client can call.
//
// The `@zeroship/vite-plugin` transform replaces these exports with
// HTTP-RPC stubs in the client bundle, and registers them under
// `<modulePath>/<exportName>` on the server (so the wire path is
// `POST /_rpc/src/server/listTodos`). Calling `listTodos()` from
// client code Just Works.

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

export async function listTodos(): Promise<Todo[]> {
  return TODOS;
}
