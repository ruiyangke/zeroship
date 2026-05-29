// Browser auth API. These calls intentionally go directly to /auth/*
// instead of through RPC: login/register/logout need the browser to
// receive Set-Cookie from the control plane on the same-origin response.

export interface AuthUser {
  id: string;
  email: string;
  name: string;
  avatar_url: string | null;
}

export interface UserInfo {
  user: AuthUser;
  app: string | null;
}

export interface RegisterInput {
  email: string;
  password: string;
  name: string;
}

export interface LoginInput {
  email: string;
  password: string;
}

interface AuthUserResult {
  user: AuthUser;
}

/** Same-origin URL the browser navigates to in order to start the
 *  Google OAuth dance. The platform gateway forwards /auth/* to the
 *  control plane. Pure string manipulation — kept on the client so
 *  it stays a sync getter (callers compose it into anchor `href`s). */
export function googleStartUrl(
  returnTo: string = window.location.pathname + window.location.search,
): string {
  const safe = returnTo && returnTo.startsWith("/") && !returnTo.startsWith("//")
    ? returnTo : "/";
  const q = new URLSearchParams({ return: safe }).toString();
  return `/auth/google/start?${q}`;
}

/** Match the dashboard's AuthError shape. The server function
 *  proxy throws a plain Error with `.status` attached. */
export class AuthError extends Error {
  status: number;
  constructor(message: string, status: number) {
    super(message);
    this.name = "AuthError";
    this.status = status;
  }
}

async function readError(res: Response): Promise<string> {
  const text = await res.text().catch(() => "");
  if (!text) return res.statusText || `HTTP ${res.status}`;
  try {
    const body = JSON.parse(text) as { error?: unknown; message?: unknown };
    const message = body.error ?? body.message;
    return typeof message === "string" ? message : text;
  } catch {
    return text;
  }
}

async function authJson<T>(
  path: string,
  init: RequestInit = {},
): Promise<T> {
  const headers = new Headers(init.headers);
  if (init.body !== undefined && !headers.has("content-type")) {
    headers.set("content-type", "application/json");
  }
  const res = await fetch(path, {
    ...init,
    headers,
    credentials: "same-origin",
  });
  if (!res.ok) {
    throw new AuthError(await readError(res), res.status);
  }
  return (await res.json()) as T;
}

export async function login(input: LoginInput): Promise<AuthUserResult> {
  return authJson<AuthUserResult>("/auth/login", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

export async function register(input: RegisterInput): Promise<AuthUserResult> {
  await authJson<AuthUserResult>("/auth/register", {
    method: "POST",
    body: JSON.stringify(input),
  });
  return login({ email: input.email, password: input.password });
}

export async function logout(): Promise<{ logged_out: boolean }> {
  return authJson<{ logged_out: boolean }>("/auth/logout", {
    method: "POST",
  });
}

export async function userinfo(): Promise<UserInfo | null> {
  try {
    return await authJson<UserInfo>("/auth/userinfo");
  } catch (err) {
    if (err instanceof AuthError && err.status === 401) return null;
    throw err;
  }
}
