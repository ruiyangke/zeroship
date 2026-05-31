/**
 * Faithful test for the dev-tier auth provider — drives the REAL
 * `createDevAuthProvider` through the full `/__zeroship/auth/*` flow (authorize →
 * 302 → popup-callback relay → code exchange → session cookie → probe → mint →
 * signout). Nothing is stubbed: the provider's real HMAC cookie signing
 * (WebCrypto), real code ledger, and real wire shapes are exercised.
 *
 * Plus a production-build absence guard: the dev-auth provider must be
 * structurally absent from the prod artifacts the runtime crate `include_str!`s
 * and the prod synthetic SSR entry imports (`runtime-entry.js`, the barrel
 * `index.js`).
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

import {
  createDevAuthProvider,
  parseDevAuthConfig,
  signDevSession,
  verifyDevSession,
} from "../src/dev-auth.js";

const SECRET = "test-dev-secret-0123456789abcdef";
const DIST = resolve(dirname(fileURLToPath(import.meta.url)), "..", "dist");

/** Build a provider whose env returns our fixed secret + config. */
function makeProvider(devAuthConfig?: string) {
  const env: Record<string, string> = { ZEROSHIP_DEV_AUTH_SECRET: SECRET };
  if (devAuthConfig !== undefined) env.ZEROSHIP_DEV_AUTH = devAuthConfig;
  const provider = createDevAuthProvider((name) => env[name]);
  assert.ok(provider, "provider should be created when a secret is present");
  return provider!;
}

/** Pull the `set-cookie` token value (drops attributes). */
function cookieTokenFrom(res: Response): string | null {
  const sc = res.headers.get("set-cookie");
  if (!sc) return null;
  const pair = sc.split(";")[0].trim();
  const eq = pair.indexOf("=");
  return pair.slice(eq + 1);
}

describe("dev-auth provider — config", () => {
  test("absent/'1'/'true' → built-in default user; '0'/'false' → disabled", () => {
    assert.equal(parseDevAuthConfig(undefined)?.users[0].email, "dev@localhost");
    assert.equal(parseDevAuthConfig("1")?.users[0].id.startsWith("pws_dev"), true);
    assert.equal(parseDevAuthConfig("0"), null);
    assert.equal(parseDevAuthConfig("false"), null);
  });

  test("{ users } config keeps the granted scopes + a chosen default", () => {
    const cfg = parseDevAuthConfig(
      JSON.stringify({
        users: [
          { id: "pws_a", email: "a@x", scopes: ["openid"] },
          { id: "pws_b", email: "b@x", scopes: ["openid", "admin"] },
        ],
        defaultUserId: "pws_b",
      }),
    );
    assert.equal(cfg?.users.length, 2);
    assert.equal(cfg?.defaultUserId, "pws_b");
    assert.deepEqual(cfg?.users[1].scopes, ["openid", "admin"]);
  });
});

describe("dev-auth provider — cookie token round-trips (WebCrypto HMAC)", () => {
  test("signDevSession → verifyDevSession recovers the identity JSON", async () => {
    const userJson = JSON.stringify({ id: "pws_x", email: "x@y", email_verified: true, scopes: [] });
    const token = await signDevSession(SECRET, userJson);
    assert.equal(await verifyDevSession(SECRET, token), userJson);
  });

  test("a wrong secret is rejected (forgery guard)", async () => {
    const token = await signDevSession(SECRET, "{}");
    assert.equal(await verifyDevSession("other-secret", token), null);
  });
});

describe("dev-auth provider — full /__zeroship/auth/* flow", () => {
  test("authorize 302s to popup-callback with a code + state", async () => {
    const p = makeProvider();
    const res = await p.handle(
      new Request("http://localhost:3001/__zeroship/auth/authorize?state=st123&scope=openid"),
    );
    assert.equal(res.status, 302);
    const loc = new URL(res.headers.get("location")!);
    assert.equal(loc.pathname, "/__zeroship/auth/popup-callback");
    assert.ok(loc.searchParams.get("code"));
    assert.equal(loc.searchParams.get("state"), "st123");
  });

  test("popup-callback serves the same-origin relay page", async () => {
    const p = makeProvider();
    const res = await p.handle(new Request("http://localhost:3001/__zeroship/auth/popup-callback?code=c&state=s"));
    assert.equal(res.status, 200);
    assert.match(res.headers.get("content-type") ?? "", /text\/html/);
    const html = await res.text();
    // The relay envelope the @zeroship/auth client listens for.
    assert.match(html, /zs:authorization_response/);
    assert.match(html, /postMessage\(msg, location\.origin\)/);
  });

  test("exchange mints a session cookie + returns { user, expires_at }", async () => {
    const p = makeProvider();
    // authorize → code
    const authRes = await p.handle(new Request("http://localhost:3001/__zeroship/auth/authorize?state=s"));
    const code = new URL(authRes.headers.get("location")!).searchParams.get("code")!;

    const res = await p.handle(
      new Request("http://localhost:3001/__zeroship/auth/session", {
        method: "POST",
        headers: { "X-ZS-Auth": "1", "content-type": "application/json" },
        body: JSON.stringify({ grant_type: "authorization_code", code, code_verifier: "v" }),
      }),
    );
    assert.equal(res.status, 200);
    const body = (await res.json()) as { user: { id: string; email: string; email_verified: boolean; scopes: string[] }; expires_at: number };
    // Canonical ZeroShip-User wire shape: snake_case email_verified, pws_ id, scopes.
    assert.equal(body.user.id.startsWith("pws_"), true);
    assert.equal(body.user.email, "dev@localhost");
    assert.equal(body.user.email_verified, true);
    assert.deepEqual(body.user.scopes, ["openid", "profile", "email"]);
    assert.equal(typeof body.expires_at, "number");
    assert.ok(cookieTokenFrom(res), "exchange sets the __zeroship_dev_session cookie");
  });

  test("session probe with the cookie returns the user; without it → 401 login_required", async () => {
    const p = makeProvider();
    const authRes = await p.handle(new Request("http://localhost:3001/__zeroship/auth/authorize?state=s"));
    const code = new URL(authRes.headers.get("location")!).searchParams.get("code")!;
    const exch = await p.handle(
      new Request("http://localhost:3001/__zeroship/auth/session", {
        method: "POST",
        headers: { "X-ZS-Auth": "1", "content-type": "application/json" },
        body: JSON.stringify({ code }),
      }),
    );
    const token = cookieTokenFrom(exch)!;

    const probe = await p.handle(
      new Request("http://localhost:3001/__zeroship/auth/session", {
        headers: { cookie: `__zeroship_dev_session=${token}` },
      }),
    );
    assert.equal(probe.status, 200);
    const body = (await probe.json()) as { user: { email: string } };
    assert.equal(body.user.email, "dev@localhost");

    const anon = await p.handle(new Request("http://localhost:3001/__zeroship/auth/session"));
    assert.equal(anon.status, 401);
    assert.equal(((await anon.json()) as { error: string }).error, "login_required");
  });

  test("?mint=1 re-issues a fresh cookie", async () => {
    const p = makeProvider();
    const token = await signDevSession(
      SECRET,
      JSON.stringify({ id: "pws_x", email: "x@y", name: "X", email_verified: true, scopes: [] }),
    );
    const res = await p.handle(
      new Request("http://localhost:3001/__zeroship/auth/session?mint=1", {
        headers: { "X-ZS-Auth": "1", cookie: `__zeroship_dev_session=${token}` },
      }),
    );
    assert.equal(res.status, 200);
    assert.ok(cookieTokenFrom(res), "mint re-sets the cookie");
  });

  test("signout is 204 + clears the cookie (idempotent)", async () => {
    const p = makeProvider();
    const res = await p.handle(
      new Request("http://localhost:3001/__zeroship/auth/signout", {
        method: "POST",
        headers: { "X-ZS-Auth": "1", "content-type": "application/json" },
        body: JSON.stringify({ scope: "local" }),
      }),
    );
    assert.equal(res.status, 204);
    assert.match(res.headers.get("set-cookie") ?? "", /Max-Age=0/);
  });

  test("multi-user config without a pick renders the dev picker", async () => {
    const p = makeProvider(JSON.stringify({ users: [{ id: "pws_a", name: "A" }, { id: "pws_b", name: "B" }] }));
    const res = await p.handle(new Request("http://localhost:3001/__zeroship/auth/authorize?state=s"));
    assert.equal(res.status, 200);
    assert.match(res.headers.get("content-type") ?? "", /text\/html/);
    const html = await res.text();
    assert.match(html, /Choose a dev user/);
    assert.match(html, /pws_a/);
    assert.match(html, /pws_b/);
  });

  test("disabled when no secret is present", () => {
    assert.equal(createDevAuthProvider(() => undefined), null);
  });
});

describe("dev-auth provider — DEV-ONLY by construction (build artifact guard)", () => {
  // The dev provider must never reach a production .zship. The runtime crate
  // include_str!s `runtime-entry.js`; the prod synthetic SSR entry imports the
  // barrel `index.js` for its side effects. Neither may carry the dev provider.
  for (const artifact of ["runtime-entry.js", "index.js", "dispatcher.js"]) {
    test(`dist/${artifact} contains no dev-auth provider symbols`, () => {
      const src = readFileSync(resolve(DIST, artifact), "utf8");
      // Strip line comments so the deliberate explanatory comment in index.js
      // (which mentions "dev-auth" by name) is not a false positive.
      const code = src
        .split("\n")
        .filter((line) => !line.trim().startsWith("//"))
        .join("\n");
      assert.doesNotMatch(code, /createDevAuthProvider/, `${artifact} must not reference createDevAuthProvider`);
      assert.doesNotMatch(code, /__zeroship_dev_session/, `${artifact} must not reference the dev session cookie`);
      assert.doesNotMatch(code, /signDevSession/, `${artifact} must not reference signDevSession`);
      assert.doesNotMatch(code, /\/__zeroship\/auth\/authorize/, `${artifact} must not embed the dev authorize route`);
    });
  }
});
