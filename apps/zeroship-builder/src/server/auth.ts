"use server";
// Auth server functions — proxy to control plane's /auth/* with
// cookie passthrough. The dashboard React layer calls these as
// plain async functions; the vite-plugin turns them into RPC.
//
// Cookies flow: browser ↔ zeroship-builder app ↔ control plane.
// The builder app is on the same origin as the dashboard UI, so
// the browser sends `__zs_session` to us; we forward it to the
// control plane and propagate any Set-Cookie headers back.

import { CONTROL_URL } from "./env";
import { getRequest, getResponseHeaders } from "./request-context";

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

/** Forward a request to the control plane, pulling the request's
 *  cookies through and propagating any Set-Cookie headers back.
 *  All auth + admin routes go through this. */
async function proxy<T>(
  path: string,
  init: { method?: string; body?: unknown } = {},
): Promise<T> {
  const req = getRequest();
  const cookie = req?.headers.get("cookie") ?? "";

  const upstream = await fetch(`${CONTROL_URL()}${path}`, {
    method: init.method ?? "GET",
    headers: {
      "content-type": "application/json",
      ...(cookie ? { cookie } : {}),
    },
    body: init.body !== undefined ? JSON.stringify(init.body) : undefined,
  });

  // Mirror Set-Cookie headers back through to the browser.
  const respHeaders = getResponseHeaders();
  if (respHeaders) {
    for (const [k, v] of upstream.headers.entries()) {
      if (k.toLowerCase() === "set-cookie") {
        respHeaders.append("Set-Cookie", v);
      }
    }
  }

  if (upstream.status === 204) return undefined as T;
  const body = await upstream.json();
  if (!upstream.ok) {
    const msg = (body && typeof body === "object" && "error" in body)
      ? String(body.error)
      : `HTTP ${upstream.status}`;
    const err = new Error(msg) as Error & { status: number };
    err.status = upstream.status;
    throw err;
  }
  return body as T;
}

export async function register(input: {
  email: string;
  password: string;
  name: string;
}): Promise<{ user: AuthUser }> {
  return proxy("/auth/register", { method: "POST", body: input });
}
register.config = { id: "auth.register" };

export async function login(input: {
  email: string;
  password: string;
}): Promise<{ user: AuthUser }> {
  return proxy("/auth/login", { method: "POST", body: input });
}
login.config = { id: "auth.login" };

export async function logout(): Promise<{ logged_out: boolean }> {
  return proxy("/auth/logout", { method: "POST" });
}
logout.config = { id: "auth.logout" };

export async function userinfo(): Promise<UserInfo | null> {
  try {
    return await proxy<UserInfo>("/auth/userinfo");
  } catch (e) {
    if ((e as { status?: number }).status === 401) return null;
    throw e;
  }
}
userinfo.config = { id: "auth.userinfo" };

