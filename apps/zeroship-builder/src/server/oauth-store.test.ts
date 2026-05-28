"use server";

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@zeroship/kv", () => ({
  kv: {
    get: async () => {
      throw new Error("kv unavailable in unit tests");
    },
    set: async () => ({ data: { ok: true }, error: null }),
    delete: async () => ({ data: { deleted: true }, error: null }),
  },
}));

import { persistGet } from "./internal/persist.js";
import {
  deleteTokens,
  loadTokens,
  saveTokens,
  tokenStorageKey,
  type StoredTokens,
} from "./oauth-store";

const ENV_KEYS = [
  "BUILDER_INSTANCE_ID",
  "BUILDER_TOKEN_ENCRYPTION_KEY",
] as const;

const originalEnv = new Map(ENV_KEYS.map((key) => [key, process.env[key]]));

describe("oauth-store", () => {
  beforeEach(() => {
    process.env.BUILDER_INSTANCE_ID = "builder-test";
    process.env.BUILDER_TOKEN_ENCRYPTION_KEY = "test-key-material-that-is-long-enough";
    vi.spyOn(console, "warn").mockImplementation(() => {});
  });

  afterEach(() => {
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

  it("oauth-store encrypts at rest; load round-trips; delete clears", async () => {
    const userId = "usr_oauth_store_roundtrip";
    const tokens: StoredTokens = {
      access_token: "access-secret-token",
      refresh_token: "refresh-secret-token",
      expires_at: 4_102_444_800,
      scope: "apps:read env:read",
    };

    await deleteTokens(userId);
    await saveTokens(userId, tokens);

    const raw = await persistGet<unknown>(tokenStorageKey(userId), null);
    const serialized = JSON.stringify(raw);
    expect(raw).not.toBeNull();
    expect(serialized).not.toContain(tokens.access_token);
    expect(serialized).not.toContain(tokens.refresh_token);

    await expect(loadTokens(userId)).resolves.toEqual(tokens);

    await deleteTokens(userId);
    await expect(loadTokens(userId)).resolves.toBeNull();
  });

  it("oauth-store rejects load with wrong encryption key", async () => {
    const userId = "usr_oauth_store_wrong_key";
    const tokens: StoredTokens = {
      access_token: "access-token-wrong-key",
      refresh_token: "refresh-token-wrong-key",
      expires_at: 4_102_444_800,
      scope: "apps:read",
    };

    await deleteTokens(userId);
    await saveTokens(userId, tokens);

    process.env.BUILDER_TOKEN_ENCRYPTION_KEY = "different-key-material";
    await expect(loadTokens(userId)).rejects.toThrow();

    process.env.BUILDER_TOKEN_ENCRYPTION_KEY = "test-key-material-that-is-long-enough";
    await deleteTokens(userId);
  });
});
