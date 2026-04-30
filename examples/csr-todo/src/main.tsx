// Client entry — bootstrap React at /, render a primitive client-side
// "router" so links inside the SPA don't trigger full reloads.
//
// Phase 5: mount `<ZeroshipProvider>` at the root. The provider:
//   - wraps `<QueryClientProvider>` for React Query,
//   - stashes the QueryClient on `_hookRegistry.queryClient` so
//     `listTodos.invalidate()` and `rpcInvalidate("todos.")` work
//     without threading the client through every call site,
//   - importing `@zeroship/rpc-react` side-effect-populates the
//     hooks registry so `listTodos.useQuery(...)` resolves.
import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { QueryClient } from "@tanstack/react-query";
import { ZeroshipProvider } from "@zeroship/rpc-react";
import { App } from "./components/App";
import { About } from "./components/About";

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      // Spec §10: matches typical resource policy.cache.max_age. Per-
      // procedure overrides come from the manifest at build time.
      staleTime: 30_000,
      retry: (n, err) => {
        const e = err as { retryable?: boolean };
        return Boolean(e?.retryable) && n < 3;
      },
    },
  },
});

function Router() {
  const [path, setPath] = useState(window.location.pathname);
  // Listen for back/forward to keep state in sync with URL.
  React.useEffect(() => {
    const onPop = () => setPath(window.location.pathname);
    window.addEventListener("popstate", onPop);
    return () => window.removeEventListener("popstate", onPop);
  }, []);
  const navigate = (to: string) => {
    window.history.pushState({}, "", to);
    setPath(to);
  };
  if (path === "/about") return <About navigate={navigate} />;
  return <App navigate={navigate} />;
}

const root = document.getElementById("root")!;
createRoot(root).render(
  <ZeroshipProvider client={queryClient}>
    <Router />
  </ZeroshipProvider>,
);
