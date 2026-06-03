/**
 * Faithful dev-tier auth test — the REAL `@zeroship/auth` browser client driving
 * the REAL `@zeroship/bootstrap` dev-auth provider end-to-end, with NO gateway /
 * Hydra. This is the contract-parity proof: the same client code that talks to
 * the gateway in prod resolves a session against the dev provider's
 * `/__zeroship/auth/*` endpoints, byte-identical wire, only the backend differs.
 *
 * Nothing under test is stubbed. The harness's `FakeFetch` is wired to forward
 * every `/__zeroship/auth/*` request into `provider.handle(...)` (the real provider),
 * with a cookie bridge (the provider's `set-cookie` is stored and replayed as
 * the `cookie` header on the next request) — exactly the browser↔gateway cookie
 * relationship, just same-process.
 */

import { test, describe, beforeEach } from "node:test";
import assert from "node:assert/strict";

import { createDevAuthProvider, type DevAuthProvider } from "@zeroship/bootstrap/dev-auth";
import { createAuthClient } from "../src/client.js";
import {
  makeHarness,
  APP_ORIGIN,
  type Harness,
} from "./harness.js";

const SECRET = "dev-tier-secret-0123456789abcdef";

/**
 * Wire the harness `FakeFetch` to the real dev-auth provider, bridging cookies.
 * Returns the provider so a test can inspect it. The browser-shaped fetch the
 * client makes (`credentials:"include"`) carries the dev session cookie via our
 * jar, mirroring the gateway relationship.
 */
function wireProviderToHarness(h: Harness, devAuthConfig?: string): DevAuthProvider {
  const env: Record<string, string> = { ZEROSHIP_DEV_AUTH_SECRET: SECRET };
  if (devAuthConfig !== undefined) env.ZEROSHIP_DEV_AUTH = devAuthConfig;
  const provider = createDevAuthProvider((name) => env[name]);
  assert.ok(provider, "provider must be created");

  // Cookie bridge — the provider sets `__zeroship_dev_session` via set-cookie; we
  // store it and replay it on the next request, exactly as a browser would.
  let cookieJar = "";
  const applySetCookie = (res: Response) => {
    const sc = res.headers.get("set-cookie");
    if (!sc) return;
    const pair = sc.split(";")[0].trim();
    if (/Max-Age=0/i.test(sc)) cookieJar = "";
    else cookieJar = pair;
  };

  h.fetch.on(
    (url) => url.includes("/__zeroship/auth/"),
    async (rec) => {
      const headers: Record<string, string> = { ...rec.headers };
      if (cookieJar) headers.cookie = cookieJar;
      const init: RequestInit = { method: rec.method, headers };
      if (rec.method === "POST" && rec.body !== undefined) {
        init.body = typeof rec.body === "string" ? rec.body : JSON.stringify(rec.body);
      }
      const res = await provider!.handle(new Request(rec.url, init));
      applySetCookie(res);
      return res;
    },
  );

  return provider!;
}

/**
 * Drive the popup leg the way a developer does: the client opens a popup to the
 * authorize URL → the dev provider renders the prefilled login FORM → the
 * developer clicks "Sign in" (we POST the prefilled credentials + CSRF) → the
 * provider 302s to popup-callback, whose inline script postMessages the relay
 * envelope. We follow that POST ourselves and dispatch the resulting
 * `{code,state}` — the SAME envelope the real popup-callback page emits.
 *
 * This is faithful to the new no-auto-login behaviour: there is no frictionless
 * 302; the credential POST is the load-bearing step.
 */
async function completePopup(h: Harness, provider: DevAuthProvider): Promise<void> {
  // Wait a tick for the client to open the popup + install the relay listener.
  await new Promise((r) => setTimeout(r, 0));
  const popup = h.window.lastOpened;
  assert.ok(popup, "client should have opened a popup");
  const authorizeUrl = popup!.location.href || h.window.location.href;
  assert.ok(authorizeUrl.includes("/__zeroship/auth/authorize"), `popup should navigate to authorize, got ${authorizeUrl}`);

  // 1. GET the login form (no auto-login — this is a 200 HTML form now).
  const formRes = await provider.handle(new Request(authorizeUrl));
  assert.equal(formRes.status, 200);
  const setCookie = formRes.headers.get("set-cookie") ?? "";
  const csrfCookie = /__zeroship_dev_csrf=([^;]+)/.exec(setCookie)?.[1];
  assert.ok(csrfCookie, "form should set a CSRF cookie");
  const html = await formRes.text();
  // The developer accepts the prefilled values and submits them.
  const field = (name: string) =>
    new RegExp(`name="${name}"[^>]*value="([^"]*)"`).exec(html)?.[1] ?? "";
  const body = new URLSearchParams({
    csrf: field("csrf"),
    state: field("state"),
    redirect_uri: field("redirect_uri"),
    email: field("email"),
    password: field("password"),
  });

  // 2. POST the credentials (the "click Sign in") → 302 to the callback.
  const authRes = await provider.handle(
    new Request(`${APP_ORIGIN}/__zeroship/auth/authorize`, {
      method: "POST",
      headers: {
        "content-type": "application/x-www-form-urlencoded",
        cookie: `__zeroship_dev_csrf=${csrfCookie}`,
      },
      body: body.toString(),
    }),
  );
  assert.equal(authRes.status, 302);
  const cb = new URL(authRes.headers.get("location")!);
  const code = cb.searchParams.get("code")!;
  const state = cb.searchParams.get("state")!;
  // Emit the relay envelope exactly as popup-callback's inline script does.
  h.window.dispatchMessage({
    origin: APP_ORIGIN,
    data: { type: "zs:authorization_response", response: { code, state } },
  });
}

describe("dev-tier auth — @zeroship/auth client ↔ real dev provider", () => {
  let h: Harness;

  beforeEach(() => {
    h = makeHarness();
  });

  test("getUser() resolves the configured dev user against GET /__zeroship/auth/session", async () => {
    // Sign in through the REAL client+provider popup flow (which sets the dev
    // cookie in our jar), then probe the server-validated user via getUser().
    const provider = wireProviderToHarness(
      h,
      JSON.stringify({ user: { id: "pws_devalice", email: "alice@localhost", name: "Alice", scopes: ["openid", "profile", "email"] } }),
    );
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    const signinPromise = client.signInWithOAuth({ popup: true });
    await completePopup(h, provider);
    await signinPromise;

    const user = await client.getUser();
    assert.ok(user, "getUser should resolve a dev user");
    assert.equal(user!.id, "pws_devalice");
    assert.equal(user!.email, "alice@localhost");
    assert.equal(user!.name, "Alice");
    assert.equal(user!.emailVerified, true); // snake→camel normalization
    assert.deepEqual(user!.scopes, ["openid", "profile", "email"]);
  });

  test("getUser() returns null when there is no dev session (anonymous)", async () => {
    wireProviderToHarness(h);
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    const user = await client.getUser();
    assert.equal(user, null);
  });

  test("signInWithOAuth() completes the full popup flow → Session", async () => {
    const provider = wireProviderToHarness(h);
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);

    const signinPromise = client.signInWithOAuth({ popup: true });
    await completePopup(h, provider);
    const session = await signinPromise;

    assert.ok(session.user.id.startsWith("pws_"), "dev user id is a pws_ pairwise subject");
    assert.equal(session.user.email, "dev@localhost");
    assert.equal(session.user.emailVerified, true);
    assert.ok(session.expires_at > Math.floor(Date.now() / 1000));

    // getSession() is now cache-only and reflects the just-signed-in identity.
    const cached = await client.getSession();
    assert.equal(cached?.user.id, session.user.id);
    assert.equal(client.isAuthenticated(), true);
  });

  test("signOut() clears the session; subsequent getUser() is anonymous", async () => {
    const provider = wireProviderToHarness(h);
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);

    const signinPromise = client.signInWithOAuth({ popup: true });
    await completePopup(h, provider);
    await signinPromise;
    assert.equal(client.isAuthenticated(), true);

    await client.signOut();
    assert.equal(client.isAuthenticated(), false);
    const user = await client.getUser();
    assert.equal(user, null);
  });
});
