"use server";

import { Buffer } from "node:buffer";
import crypto from "node:crypto";
import { refreshAccessToken, type TokenResponse } from "./oauth.js";
import { persistDelete, persistGet, persistSet } from "./internal/persist.js";

const STORE_VERSION = 1;
const TOKEN_REFRESH_SKEW_SECONDS = 60;

export interface StoredTokens {
  access_token: string;
  refresh_token: string;
  expires_at: number;
  scope: string;
}

interface EncryptedTokenRecord {
  v: typeof STORE_VERSION;
  alg: "AES-256-GCM";
  iv: string;
  tag: string;
  ciphertext: string;
}

export async function saveTokens(userId: string, tokens: StoredTokens): Promise<void> {
  const key = tokenStorageKey(userId);
  await persistSet(key, encryptTokens(key, tokens));
}

export async function loadTokens(userId: string): Promise<StoredTokens | null> {
  const key = tokenStorageKey(userId);
  const record = await persistGet<EncryptedTokenRecord | null>(key, null);
  if (!record) return null;
  return decryptTokens(key, record);
}

export async function loadFreshTokens(userId: string): Promise<StoredTokens | null> {
  const tokens = await loadTokens(userId);
  if (!tokens) return null;

  if (tokens.expires_at > unixSeconds() + TOKEN_REFRESH_SKEW_SECONDS) {
    return tokens;
  }

  const refreshed = await refreshAccessToken(tokens.refresh_token);
  const next = storedTokensFromResponse(refreshed);
  await saveTokens(userId, next);
  return next;
}

export async function deleteTokens(userId: string): Promise<void> {
  await persistDelete(tokenStorageKey(userId));
}

export function storedTokensFromResponse(
  tokens: TokenResponse,
  nowSeconds: number = unixSeconds(),
): StoredTokens {
  return {
    access_token: tokens.access_token,
    refresh_token: tokens.refresh_token,
    expires_at: nowSeconds + tokens.expires_in,
    scope: tokens.scope,
  };
}

export function tokenStorageKey(userId: string): string {
  if (!userId) throw new Error("userId is required");
  return `builder:oauth:${builderInstanceId()}:tokens:${userId}`;
}

function encryptTokens(key: string, tokens: StoredTokens): EncryptedTokenRecord {
  const iv = crypto.randomBytes(12);
  const cipher = crypto.createCipheriv("aes-256-gcm", deriveEncryptionKey(), iv);
  cipher.setAAD(Buffer.from(key, "utf8"));

  const ciphertext = Buffer.concat([
    cipher.update(JSON.stringify(tokens), "utf8"),
    cipher.final(),
  ]);

  return {
    v: STORE_VERSION,
    alg: "AES-256-GCM",
    iv: iv.toString("base64url"),
    tag: cipher.getAuthTag().toString("base64url"),
    ciphertext: ciphertext.toString("base64url"),
  };
}

function decryptTokens(key: string, record: EncryptedTokenRecord): StoredTokens {
  if (record.v !== STORE_VERSION || record.alg !== "AES-256-GCM") {
    throw new Error("unsupported encrypted token record");
  }

  const decipher = crypto.createDecipheriv(
    "aes-256-gcm",
    deriveEncryptionKey(),
    Buffer.from(record.iv, "base64url"),
  );
  decipher.setAAD(Buffer.from(key, "utf8"));
  decipher.setAuthTag(Buffer.from(record.tag, "base64url"));

  const plaintext = Buffer.concat([
    decipher.update(Buffer.from(record.ciphertext, "base64url")),
    decipher.final(),
  ]).toString("utf8");

  return parseStoredTokens(JSON.parse(plaintext));
}

function parseStoredTokens(value: unknown): StoredTokens {
  if (!value || typeof value !== "object") {
    throw new Error("stored OAuth token payload was not an object");
  }
  const body = value as Record<string, unknown>;
  const accessToken = body.access_token;
  const refreshToken = body.refresh_token;
  const expiresAt = body.expires_at;
  const scope = body.scope;
  if (typeof accessToken !== "string" || accessToken.length === 0) {
    throw new Error("stored OAuth token payload access_token was missing");
  }
  if (typeof refreshToken !== "string" || refreshToken.length === 0) {
    throw new Error("stored OAuth token payload refresh_token was missing");
  }
  if (typeof expiresAt !== "number" || !Number.isFinite(expiresAt)) {
    throw new Error("stored OAuth token payload expires_at was invalid");
  }
  if (typeof scope !== "string") {
    throw new Error("stored OAuth token payload scope was invalid");
  }
  return {
    access_token: accessToken,
    refresh_token: refreshToken,
    expires_at: expiresAt,
    scope,
  };
}

function deriveEncryptionKey(): Buffer {
  const secret = readEnv("BUILDER_TOKEN_ENCRYPTION_KEY", "");
  if (!secret) {
    throw new Error("BUILDER_TOKEN_ENCRYPTION_KEY is required");
  }

  return Buffer.from(crypto.hkdfSync(
    "sha256",
    Buffer.from(secret, "utf8"),
    Buffer.from("zeroship-builder-oauth-store:v1", "utf8"),
    Buffer.from("token-encryption", "utf8"),
    32,
  ));
}

function builderInstanceId(): string {
  return readEnv("BUILDER_INSTANCE_ID", "local");
}

function unixSeconds(): number {
  return Math.floor(Date.now() / 1000);
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
