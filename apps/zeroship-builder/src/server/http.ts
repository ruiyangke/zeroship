"use server";

import { Buffer } from "node:buffer";
import crypto from "node:crypto";
import {
  buildAuthorizationUrl,
  exchangeCode,
  generatePkce,
  revokeToken,
  subjectFromAccessToken,
} from "./oauth.js";
import {
  deleteTokens,
  loadTokens,
  saveTokens,
  storedTokensFromResponse,
} from "./oauth-store.js";
import { previewFetch } from "./preview-proxy.js";

const STATE_COOKIE = "__zs_builder_oauth_state";
const PKCE_COOKIE = "__zs_builder_oauth_pkce";
const RETURN_COOKIE = "__zs_builder_oauth_return";
const USER_COOKIE = "__zs_builder_oauth_user";
const TRANSIENT_MAX_AGE_SECONDS = 10 * 60;

export async function builderFetch(
  request: Request,
  env: unknown,
  ctx: unknown,
): Promise<Response> {
  const url = new URL(request.url);

  if (url.pathname === "/auth/login") {
    if (request.method !== "GET") return methodNotAllowed("GET");
    return oauthLogin(request);
  }

  if (url.pathname === "/auth/callback") {
    if (request.method !== "GET") return methodNotAllowed("GET");
    return oauthCallback(request);
  }

  if (url.pathname === "/auth/logout") {
    if (request.method !== "POST") return methodNotAllowed("POST");
    return oauthLogout(request);
  }

  void env;
  void ctx;
  return previewFetch(request);
}

function oauthLogin(request: Request): Response {
  const url = new URL(request.url);
  const pkce = generatePkce();
  const state = crypto.randomBytes(32).toString("base64url");
  const returnTo = sanitizeReturn(url.searchParams.get("return"));

  const res = redirect(buildAuthorizationUrl({ state, pkce }));
  setCookie(res.headers, request, STATE_COOKIE, state, {
    maxAge: TRANSIENT_MAX_AGE_SECONDS,
    path: "/auth",
  });
  setCookie(res.headers, request, PKCE_COOKIE, pkce.verifier, {
    maxAge: TRANSIENT_MAX_AGE_SECONDS,
    path: "/auth",
  });
  setCookie(res.headers, request, RETURN_COOKIE, returnTo, {
    maxAge: TRANSIENT_MAX_AGE_SECONDS,
    path: "/auth",
  });
  return res;
}

async function oauthCallback(request: Request): Promise<Response> {
  const url = new URL(request.url);
  const cookies = parseCookies(request.headers.get("cookie") ?? "");
  const res = redirect("/home");
  clearTransientCookies(res.headers, request);

  const providerError = url.searchParams.get("error");
  if (providerError) {
    res.headers.set("Location", loginError(providerError));
    return res;
  }

  const expectedState = cookies.get(STATE_COOKIE);
  const actualState = url.searchParams.get("state");
  if (!expectedState || !actualState || expectedState !== actualState) {
    res.headers.set("Location", loginError("oauth_state"));
    return res;
  }

  const code = url.searchParams.get("code");
  const verifier = cookies.get(PKCE_COOKIE);
  if (!code || !verifier) {
    res.headers.set("Location", loginError("oauth_code"));
    return res;
  }

  try {
    const tokenResponse = await exchangeCode({ code, pkceVerifier: verifier });
    const userId = subjectFromAccessToken(tokenResponse.access_token);
    if (!userId) throw new Error("access token missing subject");

    await saveTokens(userId, storedTokensFromResponse(tokenResponse));
    setCookie(res.headers, request, USER_COOKIE, signUserCookie(userId), {
      maxAge: 90 * 24 * 60 * 60,
      path: "/",
    });
    res.headers.set("Location", cookies.get(RETURN_COOKIE) ?? "/home");
  } catch {
    res.headers.set("Location", loginError("oauth_exchange"));
  }

  return res;
}

async function oauthLogout(request: Request): Promise<Response> {
  const cookies = parseCookies(request.headers.get("cookie") ?? "");
  const userId = verifyUserCookie(cookies.get(USER_COOKIE) ?? "");

  if (userId) {
    const tokens = await loadTokens(userId);
    if (tokens) {
      await Promise.allSettled([
        revokeToken(tokens.refresh_token, "refresh_token"),
        revokeToken(tokens.access_token, "access_token"),
      ]);
    }
    await deleteTokens(userId);
  }

  const headers = new Headers();
  clearCookie(headers, request, USER_COOKIE, "/");
  return new Response(null, { status: 204, headers });
}

function redirect(location: string): Response {
  return new Response(null, {
    status: 302,
    headers: { location },
  });
}

function methodNotAllowed(allowed: string): Response {
  return new Response("Method Not Allowed", {
    status: 405,
    headers: { allow: allowed },
  });
}

function loginError(error: string): string {
  const params = new URLSearchParams({ error });
  return `/login?${params.toString()}`;
}

function sanitizeReturn(raw: string | null): string {
  if (!raw) return "/home";
  if (raw.startsWith("//") || raw.includes("://") || !raw.startsWith("/")) return "/home";
  return raw;
}

function clearTransientCookies(headers: Headers, request: Request): void {
  clearCookie(headers, request, STATE_COOKIE, "/auth");
  clearCookie(headers, request, PKCE_COOKIE, "/auth");
  clearCookie(headers, request, RETURN_COOKIE, "/auth");
}

function setCookie(
  headers: Headers,
  request: Request,
  name: string,
  value: string,
  opts: { maxAge: number; path: string },
): void {
  const parts = [
    `${name}=${encodeURIComponent(value)}`,
    `Max-Age=${opts.maxAge}`,
    `Path=${opts.path}`,
    "HttpOnly",
    "SameSite=Lax",
  ];
  if (isSecure(request)) parts.push("Secure");
  headers.append("Set-Cookie", parts.join("; "));
}

function clearCookie(headers: Headers, request: Request, name: string, path: string): void {
  const parts = [
    `${name}=`,
    "Max-Age=0",
    `Path=${path}`,
    "HttpOnly",
    "SameSite=Lax",
  ];
  if (isSecure(request)) parts.push("Secure");
  headers.append("Set-Cookie", parts.join("; "));
}

function parseCookies(cookieHeader: string): Map<string, string> {
  const cookies = new Map<string, string>();
  for (const part of cookieHeader.split(";")) {
    const trimmed = part.trim();
    if (!trimmed) continue;
    const eq = trimmed.indexOf("=");
    if (eq <= 0) continue;
    const name = trimmed.slice(0, eq);
    const value = trimmed.slice(eq + 1);
    try {
      cookies.set(name, decodeURIComponent(value));
    } catch {
      cookies.set(name, value);
    }
  }
  return cookies;
}

function isSecure(request: Request): boolean {
  const url = new URL(request.url);
  return url.protocol === "https:" ||
    request.headers.get("x-forwarded-proto")?.toLowerCase() === "https";
}

function signUserCookie(userId: string): string {
  const sig = crypto
    .createHmac("sha256", cookieSecret())
    .update(userId)
    .digest("base64url");
  return `${Buffer.from(userId, "utf8").toString("base64url")}.${sig}`;
}

function verifyUserCookie(value: string): string | null {
  const [encodedUserId, sig] = value.split(".");
  if (!encodedUserId || !sig) return null;

  try {
    const userId = Buffer.from(encodedUserId, "base64url").toString("utf8");
    const expected = crypto
      .createHmac("sha256", cookieSecret())
      .update(userId)
      .digest("base64url");
    const a = Buffer.from(sig, "utf8");
    const b = Buffer.from(expected, "utf8");
    if (a.length !== b.length || !crypto.timingSafeEqual(a, b)) return null;
    return userId;
  } catch {
    return null;
  }
}

function cookieSecret(): string {
  const secret = readEnv(
    "BUILDER_COOKIE_SECRET",
    readEnv("BUILDER_TOKEN_ENCRYPTION_KEY", ""),
  );
  if (!secret) throw new Error("BUILDER_TOKEN_ENCRYPTION_KEY is required");
  return secret;
}

function readEnv(key: string, fallback: string): string {
  const proc = (globalThis as {
    process?: { env?: Record<string, string | undefined> };
  }).process;
  const fromProcess = proc?.env?.[key];
  if (typeof fromProcess === "string" && fromProcess.length > 0) return fromProcess;

  const fromRuntime = (globalThis as { env?: Record<string, string | undefined> }).env?.[key];
  if (typeof fromRuntime === "string" && fromRuntime.length > 0) return fromRuntime;

  return fallback;
}
