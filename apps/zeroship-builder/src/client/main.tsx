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
// mount-time session recovery. `appOrigin` defaults to `location.origin`;
// we pin the console scope set explicitly.
//
// IMMERSIVE LOGIN (Stripe-Elements model). The console enables the
// in-page password login iframe (`immersive: true`). The SDK only chooses
// the iframe when `eTLD+1(appOrigin) === eTLD+1(authOrigin)` (§6.5) — the
// console is `console.zeroship.ai`, the auth service is `auth.zeroship.ai`,
// both same-site under `zeroship.ai`, so the iframe is a first-party
// context (cookies not partitioned). A non-same-site surface falls back to
// the popup automatically. In dev there is no separate `auth.` origin —
// the dev-auth provider is served same-origin through the vite-plugin
// proxy — so `authOrigin` is `location.origin` (trivially same-site, the
// framed login is fillable in-frame). Federated providers (Google) always
// stay a popup window; only the first-party password UI gets the iframe.
const AUTH_ORIGIN = import.meta.env.DEV
  ? window.location.origin
  : "https://auth.zeroship.ai";

// The root ErrorBoundary catches any render-phase crash so the app
// shows an editorial wall instead of a blank white page. Per-canvas
// boundaries inside WorkspaceShell stop a single canvas crash from
// blanking the workspace.
createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <ErrorBoundary label="the app">
      <QueryClientProvider client={queryClient}>
        <AuthProvider
          options={{
            scope: ["openid", "profile", "email"],
            immersive: true,
            authOrigin: AUTH_ORIGIN,
          }}
        >
          <ThemeProvider defaultTheme="crystal">
            <App />
          </ThemeProvider>
        </AuthProvider>
      </QueryClientProvider>
    </ErrorBoundary>
  </StrictMode>,
);
