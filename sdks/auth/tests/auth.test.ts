import { test, describe } from "node:test";
import assert from "node:assert/strict";

// We need to set up mocks before importing the SDK
// because it checks typeof zeroship at call time

const mockUser = {
  id: "usr_0Bk3Np4qR5sT7uV8wYz1A",
  email: "alice@example.com",
  name: "Alice Smith",
  avatar: null,
};

describe("auth — server context (zeroship global)", () => {
  test("getUser returns user from native context", async () => {
    (globalThis as any).zeroship = {
      auth: {
        getUser: () => mockUser,
        requireUser: () => mockUser,
      },
    };
    // Dynamic import to pick up the global
    const { auth } = await import("../src/index.js");

    const user = auth.getUser();
    assert.equal(user?.id, "usr_0Bk3Np4qR5sT7uV8wYz1A");
    assert.equal(user?.email, "alice@example.com");
    assert.equal(user?.name, "Alice Smith");
    assert.equal(user?.avatar, null);
  });

  test("requireUser returns user from native context", async () => {
    (globalThis as any).zeroship = {
      auth: {
        getUser: () => mockUser,
        requireUser: () => mockUser,
      },
    };
    const { auth } = await import("../src/index.js");

    const user = auth.requireUser();
    assert.equal(user.id, "usr_0Bk3Np4qR5sT7uV8wYz1A");
  });

  test("isLoggedIn returns true when authenticated", async () => {
    (globalThis as any).zeroship = {
      auth: {
        getUser: () => mockUser,
        requireUser: () => mockUser,
      },
    };
    const { auth } = await import("../src/index.js");

    assert.equal(auth.isLoggedIn(), true);
  });

  test("getUser returns null when native returns null", async () => {
    (globalThis as any).zeroship = {
      auth: {
        getUser: () => null,
        requireUser: () => { throw new Error("Authentication required"); },
      },
    };
    const { auth } = await import("../src/index.js");

    assert.equal(auth.getUser(), null);
    assert.equal(auth.isLoggedIn(), false);
  });

  test("requireUser throws when native throws", async () => {
    (globalThis as any).zeroship = {
      auth: {
        getUser: () => null,
        requireUser: () => { throw new Error("Authentication required"); },
      },
    };
    const { auth } = await import("../src/index.js");

    assert.throws(() => auth.requireUser(), /Authentication required/);
  });
});

describe("auth — client context (window global)", () => {
  test("getUser reads window.__zs_user", async () => {
    delete (globalThis as any).zeroship;
    (globalThis as any).window = { __zs_user: mockUser };
    const { auth } = await import("../src/index.js");

    const user = auth.getUser();
    assert.equal(user?.id, "usr_0Bk3Np4qR5sT7uV8wYz1A");
    assert.equal(user?.email, "alice@example.com");
  });

  test("getUser returns null when window.__zs_user is null", async () => {
    delete (globalThis as any).zeroship;
    (globalThis as any).window = { __zs_user: null };
    const { auth } = await import("../src/index.js");

    assert.equal(auth.getUser(), null);
    assert.equal(auth.isLoggedIn(), false);
  });

  test("requireUser throws in client when not authenticated", async () => {
    delete (globalThis as any).zeroship;
    (globalThis as any).window = { __zs_user: null };
    const { auth } = await import("../src/index.js");

    assert.throws(() => auth.requireUser(), /Authentication required/);
  });
});

describe("auth — no context (SSR / build time)", () => {
  test("getUser returns null when neither zeroship nor window exists", async () => {
    delete (globalThis as any).zeroship;
    delete (globalThis as any).window;
    const { auth } = await import("../src/index.js");

    assert.equal(auth.getUser(), null);
    assert.equal(auth.isLoggedIn(), false);
  });

  test("requireUser throws when neither context exists", async () => {
    delete (globalThis as any).zeroship;
    delete (globalThis as any).window;
    const { auth } = await import("../src/index.js");

    assert.throws(() => auth.requireUser(), /Authentication required/);
  });
});

describe("auth — User type shape", () => {
  test("user has all expected fields", async () => {
    (globalThis as any).zeroship = {
      auth: {
        getUser: () => ({ id: "usr_test", email: "a@b.com", name: "Test", avatar: "https://img.com/a.jpg" }),
        requireUser: () => ({ id: "usr_test", email: "a@b.com", name: "Test", avatar: "https://img.com/a.jpg" }),
      },
    };
    const { auth } = await import("../src/index.js");

    const user = auth.getUser()!;
    assert.equal(typeof user.id, "string");
    assert.equal(typeof user.email, "string");
    assert.equal(typeof user.name, "string");
    assert.equal(typeof user.avatar, "string");
  });

  test("avatar can be null", async () => {
    (globalThis as any).zeroship = {
      auth: {
        getUser: () => ({ id: "usr_test", email: "a@b.com", name: "Test", avatar: null }),
        requireUser: () => ({ id: "usr_test", email: "a@b.com", name: "Test", avatar: null }),
      },
    };
    const { auth } = await import("../src/index.js");

    assert.equal(auth.getUser()!.avatar, null);
  });
});
