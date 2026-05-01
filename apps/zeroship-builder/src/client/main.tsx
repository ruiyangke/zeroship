import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import App from "./App";
import { AuthProvider } from "./auth/AuthContext";
import "./index.css";

const queryClient = new QueryClient({
  defaultOptions: { queries: { staleTime: 30_000 } },
});

// AuthProvider mounted globally so any child component can `useAuth()`.
// In dev (`isDevAutoAuth()` returns true) the provider short-circuits
// to a synthetic user — no /auth/userinfo round-trip and no login
// gate. Production-side auth flows (Login / Signup / OAuth) live in
// the orphan tree and Plan 03.2 wires them up.
createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <AuthProvider>
        <App />
      </AuthProvider>
    </QueryClientProvider>
  </StrictMode>,
);
