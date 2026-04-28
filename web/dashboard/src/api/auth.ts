// ─── Creator auth API client ────────────────────────────────────
//
// All auth calls live behind the cookie session set by the control
// plane on /auth/login (and /auth/google/callback). Browser-managed
// cookie + same-origin via the vite proxy → no Authorization header
// from the dashboard side.
//
// The legacy `localStorage.zeroship_key` master-key flow still works
// for admin tooling, but every dashboard call goes through here.

export interface AuthUser {
  id: string;
  email: string;
  name: string;
  avatar_url: string | null;
}

export interface UserInfo {
  user: AuthUser;
  /** App scope. null = creator (platform) session. */
  app: string | null;
}

const AUTH_BASE = "/auth";

async function authFetch<T>(
  path: string,
  init: RequestInit & { jsonBody?: unknown } = {},
): Promise<T> {
  const { jsonBody, ...rest } = init;
  const res = await fetch(`${AUTH_BASE}${path}`, {
    ...rest,
    credentials: "include",
    headers: {
      "Content-Type": "application/json",
      ...(rest.headers ?? {}),
    },
    body: jsonBody !== undefined ? JSON.stringify(jsonBody) : rest.body,
  });
  if (!res.ok) {
    let msg = `HTTP ${res.status}`;
    try {
      const data = await res.json();
      if (data?.error) msg = data.error;
    } catch { /* not json */ }
    throw new AuthError(msg, res.status);
  }
  if (res.status === 204) return undefined as unknown as T;
  return res.json();
}

export class AuthError extends Error {
  status: number;
  constructor(message: string, status: number) {
    super(message);
    this.name = "AuthError";
    this.status = status;
  }
}

export function register(input: { email: string; password: string; name: string }): Promise<{ user: AuthUser }> {
  return authFetch("/register", { method: "POST", jsonBody: input });
}

export function login(input: { email: string; password: string }): Promise<{ user: AuthUser }> {
  return authFetch("/login", { method: "POST", jsonBody: input });
}

export function logout(): Promise<{ logged_out: boolean }> {
  return authFetch("/logout", { method: "POST" });
}

export function userinfo(): Promise<UserInfo> {
  return authFetch("/userinfo", { method: "GET" });
}

/** Build the URL the browser should navigate to in order to start
 *  Google OAuth. Carries the current path so we land back here
 *  after the round-trip. */
export function googleStartUrl(returnTo: string = window.location.pathname + window.location.search): string {
  const q = new URLSearchParams({ return: returnTo }).toString();
  return `${AUTH_BASE}/google/start?${q}`;
}
