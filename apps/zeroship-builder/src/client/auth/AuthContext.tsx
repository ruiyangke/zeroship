// ─── AuthContext — console adapter over @zeroship/auth/react ──────
//
// The console is now a regular gateway-fronted zeroship app. End-user
// (creator) auth runs through the platform's BFF: the popup OAuth flow
// against the seeded per-app public PKCE client + the gateway's
// `/__zs/auth/*` endpoints, which custody the power token server-side
// and hand the browser an HttpOnly signed session cookie + a `{ user }`
// identity projection. NO client-held token (design
// docs/superpowers/specs/2026-05-30-console-as-regular-app-design.md
// §"End-user (creator) auth under BFF").
//
// This module is a THIN adapter over `@zeroship/auth/react`: it
// re-exports the SDK `AuthProvider` and exposes a console-shaped
// `useAuth()` so the existing call sites (TopBar / Account / AdminUsers
// / login pages) keep their `{ user, loading, logout, refresh }`
// surface while delegating entirely to the SDK client (signOut /
// checkSession). The previous bespoke `/auth/userinfo` query and the
// dev-bypass synthetic user are gone — dev and prod both run the real
// popup flow against the seeded client (the vite-plugin dev runtime
// dogfoods the same gateway path).

import { useCallback, useMemo } from "react";
import { useAuth as useSdkAuth, type User as SdkUser } from "@zeroship/auth/react";

export { AuthProvider } from "@zeroship/auth/react";

/** The console's user projection — derived from the SDK `User`. */
export interface AuthUser {
  id: string;
  email: string;
  name: string;
  avatar_url: string | null;
}

export interface AuthContextValue {
  user: AuthUser | null;
  loading: boolean;
  /** Retained no-op shim (legacy call sites): BFF auto-recovery rehydrates. */
  refresh: () => Promise<void>;
  /** Global sign-out (SDK `signOut`); clears the HttpOnly session cookie. */
  logout: () => Promise<void>;
}

/** Project the SDK `User` (nullable fields + scopes) into the console shape. */
function toConsoleUser(user: SdkUser | null): AuthUser | null {
  if (!user) return null;
  return {
    id: user.id,
    email: user.email ?? "",
    name: user.name ?? "",
    avatar_url: user.avatar,
  };
}

/**
 * Console-shaped auth snapshot, adapted from `@zeroship/auth/react`'s
 * `useAuth()`. `loading` mirrors the SDK's mount-time recovery
 * (`isLoading`); `logout` calls the SDK `signOut`; `refresh` is a retained
 * no-op shim (BFF auto-recovery rehydrates — see its body).
 */
export function useAuth(): AuthContextValue {
  const sdk = useSdkAuth();
  const user = useMemo(() => toConsoleUser(sdk.user), [sdk.user]);

  const logout = useCallback(async () => {
    await sdk.signOut();
  }, [sdk]);

  const refresh = useCallback(async () => {
    // No-op under the BFF model: the SDK `AuthProvider` runs mount-time
    // recovery (`checkSession` / the popup `exchangeCodeForSession`) and
    // mirrors every `onAuthStateChange` event into context automatically,
    // so a successful popup sign-in already publishes the authenticated
    // snapshot. Kept on the surface so the few legacy call sites that
    // awaited a manual re-query still compile and resolve.
  }, []);

  return useMemo(
    () => ({ user, loading: sdk.isLoading, refresh, logout }),
    [user, sdk.isLoading, refresh, logout],
  );
}
