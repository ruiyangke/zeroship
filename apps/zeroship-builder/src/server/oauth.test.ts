"use server";

import crypto from "node:crypto";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  buildAuthorizationUrl,
  exchangeCode,
  generatePkce,
  refreshAccessToken,
} from "./oauth";

const ENV_KEYS = [
  "HYDRA_AUTHORIZE_URL",
  "HYDRA_TOKEN_URL",
  "BUILDER_CLIENT_ID",
  "BUILDER_CLIENT_SECRET",
  "BUILDER_REDIRECT_URI",
] as const;

const originalEnv = new Map(ENV_KEYS.map((key) => [key, process.env[key]]));

describe("oauth", () => {
  beforeEach(() => {
    process.env.HYDRA_AUTHORIZE_URL = "https://auth.test/oauth2/auth";
    process.env.HYDRA_TOKEN_URL = "https://auth.test/oauth2/token";
    process.env.BUILDER_CLIENT_ID = "zeroship-builder-test";
    process.env.BUILDER_CLIENT_SECRET = "client-secret";
    process.env.BUILDER_REDIRECT_URI = "https://builder.test/auth/callback";
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
    for (const key of ENV_KEYS) {
      const original = originalEnv.get(key);
      if (original === undefined) {
        delete process.env[key];
      } else {
        process.env[key] = original;
      }
    }
  });

  it("generatePkce produces RFC 7636 challenge (base64url(SHA256(verifier)))", () => {
    const pkce = generatePkce();
    const expected = crypto
      .createHash("sha256")
      .update(pkce.verifier)
      .digest("base64url");

    expect(pkce.verifier).toMatch(/^[A-Za-z0-9_-]{43}$/);
    expect(pkce.challenge).toBe(expected);
  });

  it("buildAuthorizationUrl encodes scope, state, code_challenge", () => {
    const url = buildAuthorizationUrl({
      state: "state with spaces",
      pkce: { verifier: "verifier", challenge: "challenge/value" },
      scopes: ["apps:read", "deployments:rollback"],
    });

    expect(url).toContain("scope=apps%3Aread+deployments%3Arollback");
    expect(url).toContain("state=state+with+spaces");
    expect(url).toContain("code_challenge=challenge%2Fvalue");

    const parsed = new URL(url);
    expect(parsed.origin + parsed.pathname).toBe("https://auth.test/oauth2/auth");
    expect(parsed.searchParams.get("client_id")).toBe("zeroship-builder-test");
    expect(parsed.searchParams.get("response_type")).toBe("code");
    expect(parsed.searchParams.get("redirect_uri")).toBe("https://builder.test/auth/callback");
    expect(parsed.searchParams.get("code_challenge_method")).toBe("S256");
  });

  it("exchangeCode posts code+verifier and returns TokenResponse", async () => {
    const fetchMock = vi.fn(async () => Response.json({
      access_token: "access-one",
      refresh_token: "refresh-one",
      expires_in: 3600,
      scope: "apps:read env:read",
      token_type: "Bearer",
    }));
    vi.stubGlobal("fetch", fetchMock);

    await expect(exchangeCode({
      code: "code-one",
      pkceVerifier: "verifier-one",
    })).resolves.toEqual({
      access_token: "access-one",
      refresh_token: "refresh-one",
      expires_in: 3600,
      scope: "apps:read env:read",
      token_type: "Bearer",
    });

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [url, init] = fetchMock.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe("https://auth.test/oauth2/token");
    expect(init.method).toBe("POST");
    const headers = new Headers(init.headers);
    expect(headers.get("content-type")).toBe("application/x-www-form-urlencoded");
    expect(headers.get("authorization")).toBe(
      `Basic ${Buffer.from("zeroship-builder-test:client-secret").toString("base64")}`,
    );
    const body = init.body as URLSearchParams;
    expect(body.get("grant_type")).toBe("authorization_code");
    expect(body.get("code")).toBe("code-one");
    expect(body.get("code_verifier")).toBe("verifier-one");
    expect(body.get("redirect_uri")).toBe("https://builder.test/auth/callback");
    expect(body.get("client_id")).toBe("zeroship-builder-test");
  });

  it("refreshAccessToken rotates refresh_token", async () => {
    const fetchMock = vi.fn(async () => Response.json({
      access_token: "access-two",
      refresh_token: "refresh-two",
      expires_in: 1800,
      scope: "apps:read apps:write",
      token_type: "Bearer",
    }));
    vi.stubGlobal("fetch", fetchMock);

    const refreshed = await refreshAccessToken("refresh-one");

    expect(refreshed.refresh_token).toBe("refresh-two");
    expect(refreshed.access_token).toBe("access-two");
    const [, init] = fetchMock.mock.calls[0] as unknown as [string, RequestInit];
    const body = init.body as URLSearchParams;
    expect(body.get("grant_type")).toBe("refresh_token");
    expect(body.get("refresh_token")).toBe("refresh-one");
    expect(body.get("client_id")).toBe("zeroship-builder-test");
  });
});
