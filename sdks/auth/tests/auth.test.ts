import { test, describe, beforeEach, afterEach } from "node:test";
import assert from "node:assert/strict";
import { env } from "zeroship";

// The SDK reads the auth namespace off `env.auth`. In the Node-side
// stub (sdks/zeroship-stub) `env` is mutable, so tests inject a fake
// plugin by assigning `env.auth = { getUser, requireUser }` before
// calling into the SDK.

const mockUser = {
  id: "usr_0Bk3Np4qR5sT7uV8wYz1A",
  email: "alice@example.com",
  name: "Alice Smith",
  avatar: null,
};

function setEnvAuth(ns: Record<string, unknown> | null): void {
  if (ns === null) {
    delete (env as Record<string, unknown>).auth;
  } else {
    (env as Record<string, unknown>).auth = ns;
  }
}

describe("auth — server context (env.auth plugin)", () => {
  beforeEach(() => {
    setEnvAuth(null);
    delete (globalThis as any).window;
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

describe("auth — client context (window global)", () => {
  beforeEach(() => {
    setEnvAuth(null);
  });
  afterEach(() => {
    delete (globalThis as any).window;
  });

  test("getUser reads window.__zs_user", async () => {
    (globalThis as any).window = { __zs_user: mockUser };
    const { auth } = await import("../src/index.js");

    const user = auth.getUser();
    assert.equal(user?.id, "usr_0Bk3Np4qR5sT7uV8wYz1A");
    assert.equal(user?.email, "alice@example.com");
  });

  test("getUser returns null when window.__zs_user is null", async () => {
    (globalThis as any).window = { __zs_user: null };
    const { auth } = await import("../src/index.js");

    assert.equal(auth.getUser(), null);
    assert.equal(auth.isLoggedIn(), false);
  });

  test("requireUser throws in client when not authenticated", async () => {
    (globalThis as any).window = { __zs_user: null };
    const { auth } = await import("../src/index.js");

    assert.throws(() => auth.requireUser(), /Authentication required/);
  });
});

describe("auth — no context (SSR / build time / pre-AuthPlugin)", () => {
  beforeEach(() => {
    setEnvAuth(null);
    delete (globalThis as any).window;
  });

  test("getUser returns null when neither env.auth nor window exists", async () => {
    const { auth } = await import("../src/index.js");

    assert.equal(auth.getUser(), null);
    assert.equal(auth.isLoggedIn(), false);
  });

  test("requireUser throws when neither context exists", async () => {
    const { auth } = await import("../src/index.js");

    assert.throws(() => auth.requireUser(), /Authentication required/);
  });
});

describe("auth — User type shape", () => {
  beforeEach(() => {
    delete (globalThis as any).window;
  });

  test("user has all expected fields", async () => {
    setEnvAuth({
      getUser: () => ({
        id: "usr_test",
        email: "a@b.com",
        name: "Test",
        avatar: "https://img.com/a.jpg",
      }),
      requireUser: () => ({
        id: "usr_test",
        email: "a@b.com",
        name: "Test",
        avatar: "https://img.com/a.jpg",
      }),
    });
    const { auth } = await import("../src/index.js");

    const user = auth.getUser()!;
    assert.equal(typeof user.id, "string");
    assert.equal(typeof user.email, "string");
    assert.equal(typeof user.name, "string");
    assert.equal(typeof user.avatar, "string");
  });

  test("avatar can be null", async () => {
    setEnvAuth({
      getUser: () => ({
        id: "usr_test",
        email: "a@b.com",
        name: "Test",
        avatar: null,
      }),
      requireUser: () => ({
        id: "usr_test",
        email: "a@b.com",
        name: "Test",
        avatar: null,
      }),
    });
    const { auth } = await import("../src/index.js");

    assert.equal(auth.getUser()!.avatar, null);
  });
});
