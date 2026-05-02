// ─── AuthGuard — route gate for authed surfaces ─────────────────
//
// Wraps any route element that requires a logged-in creator.
// Behaviour:
//   loading=true       → centered italic "loading…" (one beat).
//   user||devBypass    → render <children>.
//   else               → <Navigate> to /login?return=<current-path>.
//
// In dev (vite serve) `isDevAutoAuth()` flips devBypass on, so the
// guard is transparent for local development. Production reads
// /auth/userinfo via AuthProvider and gates accordingly.

import type { ReactNode } from "react";
import { Navigate, useLocation } from "react-router-dom";
import { useAuth } from "./AuthContext";

export function AuthGuard({ children }: { children: ReactNode }) {
  const { user, loading, devBypass } = useAuth();
  const location = useLocation();

  if (loading) {
    return (
      <div
        className="min-h-screen flex items-center justify-center font-serif italic text-ink-soft text-[14px]"
        data-testid="auth-guard-loading"
      >
        loading…
      </div>
    );
  }

  if (!user && !devBypass) {
    const here = location.pathname + location.search;
    const ret = encodeURIComponent(here);
    return <Navigate to={`/login?return=${ret}`} replace />;
  }

  return <>{children}</>;
}
