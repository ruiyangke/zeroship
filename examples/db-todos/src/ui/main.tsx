import React from "react";
import { createRoot } from "react-dom/client";
import { QueryClient } from "@tanstack/react-query";
// Importing @zeroship/rpc-react side-effect-populates the hook registry so
// `listTodos.useQuery(...)` / `createTodo.useMutation(...)` resolve, and
// <ZeroshipProvider> wraps QueryClientProvider + stashes the client so
// `.setData` / `.invalidate` work off the procedure handles.
import { ZeroshipProvider } from "@zeroship/rpc-react";
import { App } from "./App";
import "./styles.css";

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: 5_000,
      refetchOnWindowFocus: false,
      retry: (n, err) => Boolean((err as { retryable?: boolean })?.retryable) && n < 2,
    },
  },
});

const el = document.getElementById("root");
if (!el) throw new Error("missing #root");
createRoot(el).render(
  <React.StrictMode>
    <ZeroshipProvider client={queryClient}>
      <App />
    </ZeroshipProvider>
  </React.StrictMode>,
);
