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
