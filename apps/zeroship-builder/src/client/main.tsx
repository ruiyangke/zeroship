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

// AuthProvider (from `@zeroship/auth/react`, re-exported by the console
// AuthContext adapter) mounted globally so any child can `useAuth()`.
// It builds one BFF AuthClient against the same-origin gateway and runs
// mount-time session recovery; the popup OAuth flow against the seeded
// per-app public PKCE client is the only login path (dev and prod
// alike). `appOrigin` defaults to `location.origin`; we pin the console
// scope set explicitly.
//
// The root ErrorBoundary catches any render-phase crash so the app
// shows an editorial wall instead of a blank white page. Per-canvas
// boundaries inside WorkspaceShell stop a single canvas crash from
// blanking the workspace.
createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <ErrorBoundary label="the app">
      <QueryClientProvider client={queryClient}>
        <AuthProvider options={{ scope: ["openid", "profile", "email"] }}>
          <ThemeProvider defaultTheme="crystal">
            <App />
          </ThemeProvider>
        </AuthProvider>
      </QueryClientProvider>
    </ErrorBoundary>
  </StrictMode>,
);
