import { test, describe, beforeEach } from "node:test";
import assert from "node:assert/strict";
import { env } from "zeroship";
import { auth } from "../src/server.js";

// The server entry (the "." export) reads the auth namespace off `env.auth`
// LAZILY on every call (never captured at import time). In the Node-side stub
// (sdks/zeroship-stub) `env` is mutable, so tests inject a fake plugin by
// assigning `env.auth = { getUser, requireUser }`. Because `auth.*` re-reads
// `env.auth` on each invocation, importing the module ONCE at the top is
// correct — there is no module-cache isolation hazard: every test sets
// `env.auth` in its own body and `auth.*` observes that fresh value.

const mockUser = {
  id: "pws_alice",
  email: "alice@relay.zeroship.ai",
  name: "Alice Smith",
  avatar: null,
  emailVerified: true,
  scopes: ["openid", "profile", "email"],
};

function setEnvAuth(ns: Record<string, unknown> | null): void {
  if (ns === null) {
    delete (env as Record<string, unknown>).auth;
  } else {
    (env as Record<string, unknown>).auth = ns;
  }
}

describe("server auth — env.auth populated", () => {
  beforeEach(() => setEnvAuth(null));

  test("getUser returns the user from env.auth", () => {
    setEnvAuth({ getUser: () => mockUser, requireUser: () => mockUser });
    const user = auth.getUser();
    assert.equal(user?.id, "pws_alice");
    assert.equal(user?.email, "alice@relay.zeroship.ai");
  });

  test("requireUser returns the user from env.auth", () => {
    setEnvAuth({ getUser: () => mockUser, requireUser: () => mockUser });
    assert.equal(auth.requireUser().id, "pws_alice");
  });

  test("isLoggedIn true when authenticated", () => {
    setEnvAuth({ getUser: () => mockUser, requireUser: () => mockUser });
    assert.equal(auth.isLoggedIn(), true);
  });

  test("getUser null when env.auth.getUser returns null", () => {
    setEnvAuth({
      getUser: () => null,
      requireUser: () => {
        throw new Error("Authentication required");
      },
    });
    assert.equal(auth.getUser(), null);
    assert.equal(auth.isLoggedIn(), false);
  });
});

describe("server auth — env.auth absent", () => {
  beforeEach(() => setEnvAuth(null));

  // This block runs AFTER the populated block above; because `auth.*` reads
  // `env.auth` lazily and `beforeEach` deletes it, these assertions prove the
  // absence path is not contaminated by a prior test that set `getUser`.
  test("getUser returns null", () => {
    assert.equal(auth.getUser(), null);
    assert.equal(auth.isLoggedIn(), false);
  });

  test("requireUser throws", () => {
    assert.throws(() => auth.requireUser(), /Authentication required/);
  });
});

describe("server auth — no client-side signOut", () => {
  beforeEach(() => setEnvAuth(null));

  // The server helper intentionally exposes NO `signOut`: the gateway only
  // registers `POST /__zs/auth/signout` (X-ZS-Auth + exact-Origin guarded),
  // which a worker handler cannot issue and a 302 redirect would 405 against.
  // Sign-out is a browser-client concern (`@zeroship/auth/client`).
  test("auth has no signOut method", () => {
    assert.equal(
      (auth as Record<string, unknown>).signOut,
      undefined,
      "server auth must not expose signOut (gateway /signout is POST-only)",
    );
  });
});

describe("server auth — getAccessToken / fetchAs (R4 power-token)", () => {
  beforeEach(() => setEnvAuth(null));

  test("getAccessToken passes ONLY {audience,scopes} and maps snake→camel", async () => {
    let received: unknown = null;
    setEnvAuth({
      getUser: () => mockUser,
      requireUser: () => mockUser,
      getAccessToken: async (opts: unknown) => {
        received = opts;
        return { access_token: "tok_abc", expires_at: 1234, scopes: ["apps:read"] };
      },
    });
    const out = await auth.getAccessToken({
      audience: "zeroship:control",
      scopes: ["apps:read"],
    });
    // The SDK forwards ONLY audience + scopes (no key, no identity).
    assert.deepEqual(received, { audience: "zeroship:control", scopes: ["apps:read"] });
    // snake_case from the op → camelCase on the public surface.
    assert.equal(out.accessToken, "tok_abc");
    assert.equal(out.expiresAt, 1234);
    assert.deepEqual(out.scopes, ["apps:read"]);
  });

  test("getAccessToken throws config_error when the op is absent", async () => {
    setEnvAuth({ getUser: () => mockUser, requireUser: () => mockUser });
    await assert.rejects(
      () => auth.getAccessToken({ audience: "zeroship:control", scopes: ["apps:read"] }),
      (err: { code?: string }) => err.code === "config_error",
    );
  });

  test("getAccessToken maps control error codes to AuthError.code", async () => {
    for (const [opCode, expected] of [
      ["forbidden_audience", "forbidden_audience"],
      ["scope_required", "scope_required"],
      ["step_up_required", "step_up_required"],
      ["unauthenticated_identity", "login_required"],
      ["unsupported_audience", "forbidden_audience"],
    ] as const) {
      setEnvAuth({
        getUser: () => mockUser,
        requireUser: () => mockUser,
        getAccessToken: async () => {
          const e = new Error("denied") as Error & { code: string };
          e.code = opCode;
          throw e;
        },
      });
      await assert.rejects(
        () => auth.getAccessToken({ audience: "zeroship:control", scopes: ["x"] }),
        (err: { code?: string }) => err.code === expected,
        `op code ${opCode} should map to ${expected}`,
      );
    }
  });

  test("fetchAs injects the minted token as a Bearer and never exposes it to the caller", async () => {
    setEnvAuth({
      getUser: () => mockUser,
      requireUser: () => mockUser,
      getAccessToken: async () => ({
        access_token: "tok_secret_xyz",
        expires_at: Math.floor(Date.now() / 1000) + 300,
        scopes: ["apps:read"],
      }),
    });
    let seenAuth: string | null = null;
    const realFetch = globalThis.fetch;
    globalThis.fetch = (async (_input: unknown, init?: { headers?: Headers }) => {
      seenAuth = init?.headers?.get("authorization") ?? null;
      return new Response("ok", { status: 200 });
    }) as typeof fetch;
    try {
      const callControl = auth.fetchAs({
        audience: "zeroship:control",
        scopes: ["apps:read"],
      });
      const res = await callControl("https://control.example/apps");
      assert.equal(res.status, 200);
      assert.equal(seenAuth, "Bearer tok_secret_xyz", "Bearer injected server-side");
    } finally {
      globalThis.fetch = realFetch;
    }
  });
});
