// ─── AuthContext — global current-user state ─────────────────────
//
// Wraps `/auth/userinfo` in TanStack Query so any component can
// `useAuth()` and get { user, loading, refresh, logout }. Mounting
// the dashboard fires one userinfo call; a 401 leaves user=null.
//
// Dev mode: `isDevAutoAuth()` short-circuits to a synthetic user so
// the dashboard never gates behind login when running locally.

import { createContext, useCallback, useContext, useMemo, type ReactNode } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  userinfo,
  logout as apiLogout,
  type AuthUser,
  AuthError,
} from "../api/auth";
import { isDevAutoAuth } from "../api";

export interface AuthContextValue {
  user: AuthUser | null;
  loading: boolean;
  /** Re-query /auth/userinfo (e.g., after login). */
  refresh: () => Promise<void>;
  /** POST /auth/logout + clear local state. */
  logout: () => Promise<void>;
  /** True in vite-dev where login is bypassed entirely. */
  devBypass: boolean;
}

const Ctx = createContext<AuthContextValue | null>(null);

const DEV_USER: AuthUser = {
  id: "usr_dev",
  email: "dev@localhost",
  name: "Dev User",
  avatar_url: null,
};

export function AuthProvider({ children }: { children: ReactNode }) {
  const devBypass = isDevAutoAuth();
  const queryClient = useQueryClient();

  const { data, isLoading, error } = useQuery({
    queryKey: ["auth", "userinfo"],
    // In dev we never call userinfo — just return a synthetic user.
    queryFn: async () => devBypass ? ({ user: DEV_USER, app: null }) : userinfo(),
    refetchOnWindowFocus: false,
    retry: (count, err) => {
      // Don't retry 401 — that's just "not logged in".
      if (err instanceof AuthError && err.status === 401) return false;
      return count < 2;
    },
  });

  const refresh = useCallback(async () => {
    await queryClient.invalidateQueries({ queryKey: ["auth", "userinfo"] });
  }, [queryClient]);

  const logout = useCallback(async () => {
    if (!devBypass) {
      try { await apiLogout(); } catch { /* best-effort */ }
    }
    queryClient.setQueryData(["auth", "userinfo"], null);
  }, [devBypass, queryClient]);

  const value: AuthContextValue = useMemo(() => {
    const user = error instanceof AuthError && error.status === 401
      ? null
      : data?.user ?? null;
    return { user, loading: isLoading, refresh, logout, devBypass };
  }, [data, error, isLoading, refresh, logout, devBypass]);

  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

export function useAuth(): AuthContextValue {
  const v = useContext(Ctx);
  if (!v) throw new Error("useAuth must be used inside AuthProvider");
  return v;
}
