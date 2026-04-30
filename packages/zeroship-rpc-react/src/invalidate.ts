// packages/zeroship-rpc-react/src/invalidate.ts
//
// `rpcInvalidate(prefix)` — bulk-invalidate every cached query whose
// id starts with the given prefix. Per spec §10:
//
//   await rpcInvalidate("todos.");   // invalidates todos.list, todos.get, ...
//
// Implementation: walks the QueryClient's QueryCache and invalidates
// each matching query. The `[wireId, input?]` queryKey shape from
// `__makeProcedure` makes this a `key[0].startsWith(prefix)` check.
//
// Pass `""` to invalidate everything (mostly useful in tests).

import { _hookRegistry } from "@zeroship/rpc-client/_hooks";
import { type QueryClient } from "@tanstack/react-query";
import { HOOK_UNAVAILABLE_MESSAGE } from "@zeroship/rpc-client";

/**
 * Invalidate every cached query whose first key segment (the wireId)
 * starts with `prefix`. Returns once React Query has finished marking
 * the matching queries as stale; an in-flight refetch may still be
 * in progress (RQ resolves the promise after the cache mutation, not
 * after the refetch settles).
 */
export async function rpcInvalidate(prefix: string): Promise<void> {
  const qc = _hookRegistry.queryClient as QueryClient | undefined;
  if (!qc) {
    throw new Error(HOOK_UNAVAILABLE_MESSAGE);
  }
  await qc.invalidateQueries({
    predicate: (query) => {
      const key = query.queryKey;
      if (!Array.isArray(key) || key.length === 0) return false;
      const head = key[0];
      if (typeof head !== "string") return false;
      return head.startsWith(prefix);
    },
  });
}
