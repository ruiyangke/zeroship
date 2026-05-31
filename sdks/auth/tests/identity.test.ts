import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { Breadcrumb, breadcrumbName } from "../src/internal/breadcrumb";
import { generatePkce, s256Challenge, generateVerifier } from "../src/internal/pkce";
import { createAuthClient } from "../src/client";
import type { Session } from "../src/types";
import {
  APP_ORIGIN,
  FakeCookies,
  FakeCrypto,
  jsonResponse,
  makeHarness,
  tokenSuccessBody,
  SESSION_EXCHANGE,
} from "./harness";

// ── BFF invariant: NO client-held token surface ───────────────────────────────
//
// The load-bearing security property of the BFF reshape: no method on the
// client hands a usable access/power token to browser JS, and the identity
// surface (Session/User) carries no token field at all. These tests drive the
// REAL client against a mocked transport (not a tautology) and assert the
// shapes + the absence of the deleted methods.

describe("BFF invariant — no client-held token surface", () => {
  test("the client exposes NO getAccessToken / getAccessTokenWithPopup", () => {
    const h = makeHarness();
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env) as unknown as Record<
      string,
      unknown
    >;
    assert.equal(
      client.getAccessToken,
      undefined,
      "getAccessToken must not exist on the BFF client",
    );
    assert.equal(
      client.getAccessTokenWithPopup,
      undefined,
      "getAccessTokenWithPopup must not exist on the BFF client",
    );
  });

  test("a signed-in Session is identity-only — no access_token/token_type/refresh_token", async () => {
    const h = makeHarness();
    h.fetch.on(SESSION_EXCHANGE, () => jsonResponse(200, tokenSuccessBody()));
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);

    const signIn = client.signInWithOAuth();
    // Drive the popup relay to completion.
    const state = await (async () => {
      for (let i = 0; i < 50; i++) {
        for (const k of h.session.map.keys()) {
          if (k.startsWith("@@zsauth@@::txn::")) return k.slice("@@zsauth@@::txn::".length);
        }
        await new Promise((r) => setTimeout(r, 1));
      }
      throw new Error("no PKCE transaction was persisted");
    })();
    h.window.dispatchMessage({
      origin: APP_ORIGIN,
      data: { type: "zs:authorization_response", response: { code: "c", state } },
    });
    const session = await signIn;

    // The exact identity-only shape — no token fields anywhere.
    assert.deepEqual(Object.keys(session).sort(), ["expires_at", "scopes", "user"]);
    const asAny = session as unknown as Record<string, unknown>;
    assert.equal("access_token" in asAny, false, "Session must not carry access_token");
    assert.equal("token_type" in asAny, false, "Session must not carry token_type");
    assert.equal("refresh_token" in asAny, false, "Session must not carry refresh_token");
    assert.equal("id_token" in asAny, false, "Session must not carry id_token");
  });

  test("getSession / getUser never expose a token", async () => {
    const h = makeHarness();
    h.fetch.on(SESSION_EXCHANGE, () => jsonResponse(200, tokenSuccessBody()));
    h.fetch.on(
      (u, m) => m === "GET" && u.includes("/__zs/auth/session") && !u.includes("mint=1"),
      () => jsonResponse(200, { user: tokenSuccessBody().user, expires_at: tokenSuccessBody().expires_at }),
    );
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);

    const signIn = client.signInWithOAuth();
    const state = await (async () => {
      for (let i = 0; i < 50; i++) {
        for (const k of h.session.map.keys()) {
          if (k.startsWith("@@zsauth@@::txn::")) return k.slice("@@zsauth@@::txn::".length);
        }
        await new Promise((r) => setTimeout(r, 1));
      }
      throw new Error("no PKCE transaction was persisted");
    })();
    h.window.dispatchMessage({
      origin: APP_ORIGIN,
      data: { type: "zs:authorization_response", response: { code: "c", state } },
    });
    await signIn;

    const s = await client.getSession();
    assert.ok(s, "getSession returns the cached identity snapshot");
    assert.equal("access_token" in (s as unknown as Record<string, unknown>), false);

    const user = await client.getUser();
    assert.ok(user, "getUser returns the server-validated identity");
    // The User projection carries id/email/scopes — never a token.
    assert.equal("access_token" in (user as unknown as Record<string, unknown>), false);
    assert.equal("token" in (user as unknown as Record<string, unknown>), false);
  });
});

// ── in-memory identity holder (getSession, cache-only, no network) ────────────

describe("in-memory identity snapshot (getSession)", () => {
  test("getSession returns null before sign-in (no snapshot, no network)", async () => {
    const h = makeHarness();
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    const before = h.fetch.requests.length;
    const s = await client.getSession();
    assert.equal(s, null);
    assert.equal(h.fetch.requests.length, before, "getSession must not hit the network");
  });

  test("getSession serves the identity snapshot set by refreshSession, no extra network", async () => {
    const h = makeHarness();
    h.fetch.on((u) => u.includes("mint=1"), () => jsonResponse(200, tokenSuccessBody()));
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    const refreshed: Session = await client.refreshSession();
    const after = h.fetch.requests.length;

    const s = await client.getSession();
    assert.equal(s?.user.id, refreshed.user.id);
    assert.deepEqual(s?.scopes, ["openid", "profile", "email"]);
    assert.equal(h.fetch.requests.length, after, "getSession reads the in-memory snapshot, no network");
  });
});

// ── breadcrumb (unchanged, no token involvement) ──────────────────────────────

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

// ── PKCE (Web Crypto, unchanged) ──────────────────────────────────────────────

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
