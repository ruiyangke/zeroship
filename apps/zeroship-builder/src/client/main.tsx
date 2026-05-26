import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { ThemeProvider } from "@zeroship/ui";
import App from "./App";
import { AuthProvider } from "./auth/AuthContext";
import { ErrorBoundary } from "./components/ErrorBoundary";
import "./index.css";
import "@zeroship/ui/styles.css";

const queryClient = new QueryClient({
  defaultOptions: { queries: { staleTime: 30_000 } },
});

// AuthProvider mounted globally so any child component can `useAuth()`.
// In dev (`isDevAutoAuth()` returns true) the provider short-circuits
// to a synthetic user — no /auth/userinfo round-trip and no login
// gate. Production-side auth flows (Login / Signup / OAuth) live in
// the orphan tree and a later cleanup wires them up.
//
// The root ErrorBoundary catches any render-phase crash so the app
// shows an editorial wall instead of a blank white page. Per-canvas
// boundaries inside WorkspaceShell stop a single canvas crash from
// blanking the workspace.
createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <ErrorBoundary label="the app">
      <QueryClientProvider client={queryClient}>
        <AuthProvider>
          <ThemeProvider defaultTheme="studio">
            <App />
          </ThemeProvider>
        </AuthProvider>
      </QueryClientProvider>
    </ErrorBoundary>
  </StrictMode>,
);
