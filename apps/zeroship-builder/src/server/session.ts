"use server";

import { Buffer } from "node:buffer";
import crypto from "node:crypto";

export const BUILDER_USER_COOKIE = "__zs_builder_oauth_user";

export function userIdFromRequest(request: Request): string | null {
  const cookies = parseCookies(request.headers.get("cookie") ?? "");
  return verifyUserCookie(cookies.get(BUILDER_USER_COOKIE) ?? "");
}

export function signUserCookie(userId: string): string {
  const sig = crypto
    .createHmac("sha256", cookieSecret())
    .update(userId)
    .digest("base64url");
  return `${Buffer.from(userId, "utf8").toString("base64url")}.${sig}`;
}

export function verifyUserCookie(value: string): string | null {
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
