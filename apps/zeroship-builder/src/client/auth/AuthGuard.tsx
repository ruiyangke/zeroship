// ─── AuthGuard — route gate for authed surfaces ─────────────────
//
// Wraps any route element that requires a logged-in creator. Reads the
// BFF session state from `@zeroship/auth/react` (via the console
// AuthContext adapter):
//   loading=true   → centered italic "loading…" (one beat).
//   user           → render <children>.
//   else           → <Navigate> to /login?return=<current-path>.
//
// No dev-bypass: dev and prod both run the real popup flow against the
// seeded per-app public PKCE client.

import type { ReactNode } from "react";
import { Navigate, useLocation } from "react-router-dom";
import { Center } from "@zeroship/ui";
import { useAuth } from "./AuthContext";
import "./AuthGuard.css";

export function AuthGuard({ children }: { children: ReactNode }) {
  const { user, loading } = useAuth();
  const location = useLocation();

  if (loading) {
    return (
      <Center minHeight="100dvh" data-testid="auth-guard-loading">
        <span className="auth-guard-loading__label">loading…</span>
      </Center>
    );
  }

  if (!user) {
    const here = location.pathname + location.search;
    const ret = encodeURIComponent(here);
    return <Navigate to={`/login?return=${ret}`} replace />;
  }

  return <>{children}</>;
}
