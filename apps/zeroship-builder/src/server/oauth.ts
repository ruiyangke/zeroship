"use server";

import { Buffer } from "node:buffer";
import crypto from "node:crypto";
import { createRemoteJWKSet, jwtVerify, type JWTVerifyOptions } from "jose";

const DEFAULT_HYDRA_AUTHORIZE_URL = "http://localhost:4444/oauth2/auth";
const DEFAULT_HYDRA_TOKEN_URL = "http://localhost:4444/oauth2/token";
const DEFAULT_HYDRA_REVOKE_URL = "http://localhost:4444/oauth2/revoke";
const DEFAULT_HYDRA_JWKS_URL = "http://localhost:4444/.well-known/jwks.json";
const DEFAULT_BUILDER_CLIENT_ID = "zeroship-builder";
const DEFAULT_BUILDER_REDIRECT_URI = "http://localhost:3001/auth/callback";

const jwksByUrl = new Map<string, ReturnType<typeof createRemoteJWKSet>>();

// Default scopes the builder needs.
export const BUILDER_SCOPES = [
  "apps:read", "apps:write", "apps:deploy",
  "env:read", "env:write",
  "secrets:read", "secrets:write",
  "deployments:read", "deployments:rollback",
] as const;

export interface PkcePair {
  verifier: string;
  challenge: string;
}

export function generatePkce(): PkcePair {
  const verifier = crypto.randomBytes(32).toString("base64url");
  return {
    verifier,
    challenge: crypto.createHash("sha256").update(verifier).digest("base64url"),
  };
}

export function buildAuthorizationUrl(opts: {
  state: string;
  pkce: PkcePair;
  scopes?: readonly string[];
}): string {
  const url = new URL(readEnv("HYDRA_AUTHORIZE_URL", DEFAULT_HYDRA_AUTHORIZE_URL));
  url.searchParams.set("client_id", builderClientId());
  url.searchParams.set("response_type", "code");
  url.searchParams.set("redirect_uri", builderRedirectUri());
  url.searchParams.set("scope", [...(opts.scopes ?? BUILDER_SCOPES)].join(" "));
  url.searchParams.set("state", opts.state);
  url.searchParams.set("code_challenge", opts.pkce.challenge);
  url.searchParams.set("code_challenge_method", "S256");
  return url.toString();
}

export interface TokenResponse {
  access_token: string;
  refresh_token: string;
  expires_in: number;
  scope: string;
  token_type: "Bearer";
}

export async function exchangeCode(opts: {
  code: string;
  pkceVerifier: string;
}): Promise<TokenResponse> {
  const body = new URLSearchParams({
    grant_type: "authorization_code",
    code: opts.code,
    redirect_uri: builderRedirectUri(),
    code_verifier: opts.pkceVerifier,
  });
  return tokenRequest(body);
}

export async function refreshAccessToken(refreshToken: string): Promise<TokenResponse> {
  const body = new URLSearchParams({
    grant_type: "refresh_token",
    refresh_token: refreshToken,
  });
  return tokenRequest(body);
}

export async function revokeToken(
  token: string,
  tokenTypeHint: "access_token" | "refresh_token" = "refresh_token",
): Promise<void> {
  const body = new URLSearchParams({
    token,
    token_type_hint: tokenTypeHint,
  });
  body.set("client_id", builderClientId());

  const res = await fetch(readEnv("HYDRA_REVOKE_URL", DEFAULT_HYDRA_REVOKE_URL), {
    method: "POST",
    headers: tokenHeaders(),
    body,
  });

  if (!res.ok) {
    throw new Error(`hydra revoke failed (${res.status}): ${await responseText(res)}`);
  }
}

export async function subjectFromAccessToken(accessToken: string): Promise<string | null> {
  try {
    const { payload } = await jwtVerify(accessToken, hydraJwks(), accessTokenVerifyOptions());
    return typeof payload.sub === "string" && payload.sub.length > 0 ? payload.sub : null;
  } catch {
    return null;
  }
}

function hydraJwks(): ReturnType<typeof createRemoteJWKSet> {
  const url = readEnv("HYDRA_JWKS_URL", DEFAULT_HYDRA_JWKS_URL);
  let jwks = jwksByUrl.get(url);
  if (!jwks) {
    jwks = createRemoteJWKSet(new URL(url));
    jwksByUrl.set(url, jwks);
  }
  return jwks;
}

function accessTokenVerifyOptions(): JWTVerifyOptions {
  const options: JWTVerifyOptions = {};
  const issuer = readEnv("HYDRA_ISSUER", "");
  if (issuer) options.issuer = issuer;
  const audience = readEnv("BUILDER_ACCESS_TOKEN_AUDIENCE", "");
  if (audience) options.audience = audience;
  return options;
}

async function tokenRequest(body: URLSearchParams): Promise<TokenResponse> {
  body.set("client_id", builderClientId());

  const res = await fetch(readEnv("HYDRA_TOKEN_URL", DEFAULT_HYDRA_TOKEN_URL), {
    method: "POST",
    headers: tokenHeaders(),
    body,
  });

  if (!res.ok) {
    throw new Error(`hydra token request failed (${res.status}): ${await responseText(res)}`);
  }

  return parseTokenResponse(await res.json());
}

function tokenHeaders(): Headers {
  const headers = new Headers({
    accept: "application/json",
    "content-type": "application/x-www-form-urlencoded",
  });

  const secret = builderClientSecret();
  if (secret) {
    const basic = Buffer.from(`${builderClientId()}:${secret}`, "utf8").toString("base64");
    headers.set("authorization", `Basic ${basic}`);
  }

  return headers;
}

function parseTokenResponse(value: unknown): TokenResponse {
  if (!value || typeof value !== "object") {
    throw new Error("hydra token response was not an object");
  }
  const body = value as Record<string, unknown>;
  const tokenType = body.token_type;
  if (tokenType !== "Bearer" && tokenType !== "bearer") {
    throw new Error("hydra token response token_type was not Bearer");
  }
  const expiresIn = body.expires_in;
  if (typeof expiresIn !== "number" || !Number.isFinite(expiresIn) || expiresIn <= 0) {
    throw new Error("hydra token response expires_in was invalid");
  }

  return {
    access_token: requiredString(body, "access_token"),
    refresh_token: requiredString(body, "refresh_token"),
    expires_in: expiresIn,
    scope: typeof body.scope === "string" ? body.scope : "",
    token_type: "Bearer",
  };
}

function requiredString(body: Record<string, unknown>, key: string): string {
  const value = body[key];
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`hydra token response ${key} was missing`);
  }
  return value;
}

async function responseText(res: Response): Promise<string> {
  try {
    const text = await res.text();
    return text || res.statusText;
  } catch {
    return res.statusText;
  }
}

function builderClientId(): string {
  return readEnv("BUILDER_CLIENT_ID", DEFAULT_BUILDER_CLIENT_ID);
}

function builderClientSecret(): string {
  return readEnv("BUILDER_CLIENT_SECRET", "");
}

function builderRedirectUri(): string {
  return readEnv("BUILDER_REDIRECT_URI", DEFAULT_BUILDER_REDIRECT_URI);
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
