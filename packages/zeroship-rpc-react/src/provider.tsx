// packages/zeroship-rpc-react/src/provider.tsx
//
// `<ZeroshipProvider client={qc}>` — wraps `<QueryClientProvider>`
// AND stashes the QueryClient on `_hookRegistry.queryClient` so the
// `proc.invalidate(...)` / `proc.prefetch(...)` / `rpcInvalidate(...)`
// helpers can resolve it without threading it through their call sites.
//
// One place to mount in the user's tree:
//
//   import { QueryClient } from "@tanstack/react-query";
//   import { ZeroshipProvider } from "@zeroship/rpc-react";
//
//   const qc = new QueryClient({ defaultOptions: { queries: { staleTime: 30_000 } } });
//   ReactDOM.createRoot(root).render(
//     <ZeroshipProvider client={qc}>
//       <App />
//     </ZeroshipProvider>
//   );
//
// The provider also imports `./hooks.js` purely for its side effect —
// importing `@zeroship/rpc-react` from the user's app pulls hooks.ts
// in transitively, which populates `_hookRegistry`. We re-import here
// so `provider.tsx` is self-contained even when called via subpath.

import * as React from "react";
import {
  type QueryClient,
  QueryClientProvider,
} from "@tanstack/react-query";
import { _hookRegistry } from "@zeroship/rpc-client/_hooks";

// Side-effect import — populates `_hookRegistry` with React Query
// hooks. Needed in case a consumer somehow imports `provider.tsx`
// directly without hitting `index.ts`.
import "./hooks.js";

export interface ZeroshipProviderProps {
  client: QueryClient;
  children: React.ReactNode;
}

/**
 * Wraps `<QueryClientProvider>` and stashes `client` on
 * `_hookRegistry.queryClient` so `proc.invalidate(...)` and friends
 * can resolve it without the call site having to thread it through.
 *
 * Note on remount semantics: stashing on a module-private singleton
 * means a second `<ZeroshipProvider>` mount with a different
 * QueryClient will overwrite the slot. That's fine — the typical
 * model is one QueryClient per app process. SSR uses a per-request
 * QueryClient; the slot ends up holding the active request's client
 * during render and is overwritten by the next request. Concurrent
 * requests render in different worker contexts (one V8 isolate per
 * app, but each render is sequential under the kernel's event-loop
 * model) so there's no cross-tenant bleed.
 */
export function ZeroshipProvider({ client, children }: ZeroshipProviderProps): React.ReactElement {
  // Stash on every render — defensive against the slot being cleared
  // by some other module. Cheap (a property write).
  _hookRegistry.queryClient = client;
  _hookRegistry.providerMounted = true;

  return (
    <QueryClientProvider client={client}>
      {children}
    </QueryClientProvider>
  );
}
