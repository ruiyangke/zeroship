import { test, describe } from "node:test";
import assert from "node:assert/strict";

import {
  CacheManager,
  InMemoryCache,
  LocalStorageCache,
} from "../src/internal/cache";
import { Breadcrumb, breadcrumbName } from "../src/internal/breadcrumb";
import { generatePkce, s256Challenge, generateVerifier } from "../src/internal/pkce";
import type { Session, User } from "../src/types";
import { APP_ORIGIN, FakeCookies, FakeCrypto, FakeStorage } from "./harness";

const USER: User = {
  id: "pws_alice",
  email: "alice@relay.zeroship.ai",
  emailVerified: true,
  name: "Alice",
  avatar: null,
  scopes: ["openid", "profile", "email"],
};

function session(over?: Partial<Session>): Session {
  return {
    access_token: "tok",
    expires_at: 10_000,
    token_type: "Bearer",
    user: USER,
    scopes: ["openid", "profile", "email"],
    ...over,
  };
}

describe("CacheManager keying + expiry", () => {
  test("token entry keys on appOrigin + scope-sorted; order does not split entries", async () => {
    const cm = new CacheManager(new InMemoryCache(), APP_ORIGIN);
    await cm.setSession(session({ scopes: ["profile", "openid", "email"] }));
    // Read back with a DIFFERENT scope order — must hit the same entry.
    const entry = await cm.getEntry(["email", "openid", "profile"], 0);
    assert.ok(entry, "scope order must not split the cache key");
    assert.equal(entry!.access_token, "tok");
  });

  test("user profile is a SEPARATE entry from the token entry", async () => {
    const backend = new InMemoryCache();
    const cm = new CacheManager(backend, APP_ORIGIN);
    await cm.setSession(session());
    const keys = backend.allKeys();
    assert.equal(keys.length, 2, `expected token + user entries, got ${keys.join(",")}`);
    assert.ok(keys.some((k) => k.endsWith("@@user@@")), "user key present");
    const user = await cm.getUser();
    assert.equal(user?.id, "pws_alice");
  });

  test("expired entry is evicted on read WHEN no refresh token", async () => {
    const cm = new CacheManager(new InMemoryCache(), APP_ORIGIN);
    await cm.setSession(session({ expires_at: 100 }));
    const got = await cm.getEntry(["openid", "profile", "email"], 200); // now > exp
    assert.equal(got, undefined, "expired entry without refresh must evict");
  });

  test("expired entry is KEPT when a refresh token is present (silent renewal)", async () => {
    const cm = new CacheManager(new InMemoryCache(), APP_ORIGIN);
    await cm.setSession(session({ expires_at: 100, refresh_token: "rt" }));
    const got = await cm.getEntry(["openid", "profile", "email"], 200);
    assert.ok(got, "expired entry WITH refresh must survive for renewal");
    assert.equal(got!.refresh_token, "rt");
  });

  test("clear() drops every entry for the app", async () => {
    const backend = new InMemoryCache();
    const cm = new CacheManager(backend, APP_ORIGIN);
    await cm.setSession(session());
    await cm.clear();
    assert.equal(backend.allKeys().length, 0);
  });

  test("toSession round-trips a cache entry", async () => {
    const cm = new CacheManager(new InMemoryCache(), APP_ORIGIN);
    const s = session({ refresh_token: "rt" });
    await cm.setSession(s);
    const entry = await cm.getEntry(s.scopes, 0);
    const back = CacheManager.toSession(entry!);
    assert.deepEqual(back, s);
  });
});

describe("cache backends", () => {
  test("InMemoryCache stores, reads, removes, lists", () => {
    const c = new InMemoryCache();
    c.set("@@zsauth@@::a", { x: 1 });
    assert.deepEqual(c.get("@@zsauth@@::a"), { x: 1 });
    assert.deepEqual(c.allKeys(), ["@@zsauth@@::a"]);
    c.remove("@@zsauth@@::a");
    assert.equal(c.get("@@zsauth@@::a"), undefined);
  });

  test("LocalStorageCache persists JSON to the backing Storage", () => {
    const storage = new FakeStorage();
    const c = new LocalStorageCache(storage);
    c.set("@@zsauth@@::k", { hello: "world" });
    // Survives a fresh wrapper over the same Storage (reload simulation).
    const c2 = new LocalStorageCache(storage);
    assert.deepEqual(c2.get("@@zsauth@@::k"), { hello: "world" });
    assert.deepEqual(c2.allKeys(), ["@@zsauth@@::k"]);
  });
});

describe("breadcrumb", () => {
  test("name is keyed on the app HOST (matches gateway anchors.rs)", () => {
    assert.equal(
      breadcrumbName("https://myapp.zeroship.ai"),
      "zs.myapp.zeroship.ai.is.authenticated",
    );
    assert.equal(
      breadcrumbName("http://myapp.localhost:3000"),
      "zs.myapp.localhost:3000.is.authenticated",
    );
  });

  test("set / isPresent / clear round-trip", () => {
    const cookies = new FakeCookies();
    const bc = new Breadcrumb(cookies, APP_ORIGIN);
    assert.equal(bc.isPresent(), false);
    bc.set();
    assert.equal(bc.isPresent(), true);
    assert.match(cookies.get(), /zs\.myapp\.zeroship\.ai\.is\.authenticated=true/);
    bc.clear();
    assert.equal(bc.isPresent(), false);
  });
});

describe("PKCE (Web Crypto)", () => {
  test("verifier is RFC-7636 unreserved; challenge is deterministic base64url", async () => {
    const crypto = new FakeCrypto();
    const v = generateVerifier(crypto);
    assert.match(v, /^[A-Za-z0-9\-._~]+$/, "verifier uses the unreserved alphabet");
    const c1 = await s256Challenge(crypto, v);
    const c2 = await s256Challenge(crypto, v);
    assert.equal(c1, c2, "S256 of a fixed verifier is stable");
    assert.match(c1, /^[A-Za-z0-9\-_]+$/, "challenge is base64url (no padding)");
    assert.ok(!c1.includes("="), "no padding");
  });

  test("generatePkce mints verifier+challenge+state+nonce", async () => {
    const p = await generatePkce(new FakeCrypto());
    assert.ok(p.verifier && p.challenge && p.state && p.nonce);
    assert.notEqual(p.state, p.nonce);
  });

  test("S256 base64url matches the RFC 7636 Appendix B vector (real SHA-256)", async () => {
    // Drive the REAL Web Crypto SHA-256 (Node exposes globalThis.crypto.subtle)
    // through our self-contained base64url encoder to prove the challenge is
    // byte-correct, independent of the deterministic FakeCrypto digest.
    const real = globalThis.crypto as unknown as Parameters<typeof s256Challenge>[0];
    const verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const challenge = await s256Challenge(real, verifier);
    assert.equal(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
  });
});
