import { test, describe, beforeEach } from "node:test";
import assert from "node:assert/strict";
import { env } from "zeroship";

// The SDK reads the auth namespace off `env.auth`. In the Node-side
// stub (sdks/zeroship-stub) `env` is mutable, so tests inject a fake
// plugin by assigning `env.auth = { getUser, requireUser }` before
// calling into the SDK.
//
// There is intentionally no browser-fallback path to exercise:
// post-Phase-3, the gateway HMAC-signs the user into the `ZeroShip-User`
// header and the runtime exposes the parsed identity via env.auth. The
// previous `window.__zs_user` fallback was deleted.

const mockUser = {
  id: "usr_0Bk3Np4qR5sT7uV8wYz1A",
  email: "alice@example.com",
  name: "Alice Smith",
  avatar: null,
  emailVerified: true,
};

function setEnvAuth(ns: Record<string, unknown> | null): void {
  if (ns === null) {
    delete (env as Record<string, unknown>).auth;
  } else {
    (env as Record<string, unknown>).auth = ns;
  }
}

describe("auth — env.auth plugin populated", () => {
  beforeEach(() => {
    setEnvAuth(null);
  });

  test("getUser returns user from env.auth", async () => {
    setEnvAuth({
      getUser: () => mockUser,
      requireUser: () => mockUser,
    });
    const { auth } = await import("../src/index.js");

    const user = auth.getUser();
    assert.equal(user?.id, "usr_0Bk3Np4qR5sT7uV8wYz1A");
    assert.equal(user?.email, "alice@example.com");
    assert.equal(user?.name, "Alice Smith");
    assert.equal(user?.avatar, null);
  });

  test("requireUser returns user from env.auth", async () => {
    setEnvAuth({
      getUser: () => mockUser,
      requireUser: () => mockUser,
    });
    const { auth } = await import("../src/index.js");

    const user = auth.requireUser();
    assert.equal(user.id, "usr_0Bk3Np4qR5sT7uV8wYz1A");
  });

  test("isLoggedIn returns true when authenticated", async () => {
    setEnvAuth({
      getUser: () => mockUser,
      requireUser: () => mockUser,
    });
    const { auth } = await import("../src/index.js");

    assert.equal(auth.isLoggedIn(), true);
  });

  test("getUser returns null when env.auth.getUser returns null", async () => {
    setEnvAuth({
      getUser: () => null,
      requireUser: () => {
        throw new Error("Authentication required");
      },
    });
    const { auth } = await import("../src/index.js");

    assert.equal(auth.getUser(), null);
    assert.equal(auth.isLoggedIn(), false);
  });

  test("requireUser throws when env.auth.requireUser throws", async () => {
    setEnvAuth({
      getUser: () => null,
      requireUser: () => {
        throw new Error("Authentication required");
      },
    });
    const { auth } = await import("../src/index.js");

    assert.throws(() => auth.requireUser(), /Authentication required/);
  });
});

describe("auth — env.auth absent (no plugin registered)", () => {
  beforeEach(() => {
    setEnvAuth(null);
  });

  test("getUser returns null", async () => {
    const { auth } = await import("../src/index.js");

    assert.equal(auth.getUser(), null);
    assert.equal(auth.isLoggedIn(), false);
  });

  test("requireUser throws", async () => {
    const { auth } = await import("../src/index.js");

    assert.throws(() => auth.requireUser(), /Authentication required/);
  });
});

describe("auth.signOut", () => {
  beforeEach(() => {
    setEnvAuth(null);
  });

  test("returns a 302 to /__zs/auth/signout", async () => {
    const { auth } = await import("../src/index.js");

    const res = auth.signOut();
    assert.equal(res.status, 302);
    assert.equal(res.headers.get("location"), "/__zs/auth/signout");
  });

  test("encodes returnTo when provided", async () => {
    const { auth } = await import("../src/index.js");

    const res = auth.signOut("/dashboard?tab=home");
    assert.equal(res.status, 302);
    assert.equal(
      res.headers.get("location"),
      "/__zs/auth/signout?return=%2Fdashboard%3Ftab%3Dhome",
    );
  });
});
