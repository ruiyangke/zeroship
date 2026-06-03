/**
 * Faithful test for the dev-tier auth provider — drives the REAL
 * `createDevAuthProvider` through the full `/__zeroship/auth/*` flow (authorize
 * login form → POST credentials → 302 → popup-callback relay → code exchange →
 * session cookie → probe → mint → signout). Nothing is stubbed: the provider's
 * real HMAC cookie signing (WebCrypto), real CSRF double-submit, real code
 * ledger, and real wire shapes are exercised.
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

/** Read a specific named cookie's value from a response's set-cookie. */
function cookieValue(res: Response, name: string): string | null {
  const sc = res.headers.get("set-cookie");
  if (!sc) return null;
  const pair = sc.split(";")[0].trim();
  const eq = pair.indexOf("=");
  return pair.slice(0, eq) === name ? pair.slice(eq + 1) : null;
}

const ORIGIN = "http://localhost:3001";

/**
 * Drive the dev login the way a developer (or browser) does: GET the prefilled
 * form, then POST it back with the CSRF cookie+field + credentials. Credentials
 * default to the built-in dev user; override to exercise the failure path.
 * Returns the POST response (302 on success, re-rendered form on failure).
 */
async function devLogin(
  p: { handle: (r: Request) => Promise<Response> },
  opts: { state?: string; email?: string; password?: string } = {},
): Promise<Response> {
  const state = opts.state ?? "s";
  const getRes = await p.handle(
    new Request(`${ORIGIN}/__zeroship/auth/authorize?state=${state}`),
  );
  const csrf = cookieValue(getRes, "__zeroship_dev_csrf");
  if (!csrf) throw new Error("GET /authorize did not set a csrf cookie");
  const body = new URLSearchParams({
    csrf,
    state,
    redirect_uri: `${ORIGIN}/__zeroship/auth/popup-callback`,
    email: opts.email ?? "dev@localhost",
    password: opts.password ?? "dev",
  });
  return p.handle(
    new Request(`${ORIGIN}/__zeroship/auth/authorize`, {
      method: "POST",
      headers: {
        "content-type": "application/x-www-form-urlencoded",
        cookie: `__zeroship_dev_csrf=${csrf}`,
      },
      body: body.toString(),
    }),
  );
}

/** Full sign-in → the exchanged session cookie token. */
async function devSessionCookie(
  p: { handle: (r: Request) => Promise<Response> },
  opts: { email?: string; password?: string } = {},
): Promise<string> {
  const loginRes = await devLogin(p, opts);
  if (loginRes.status !== 302) throw new Error(`login failed: ${loginRes.status}`);
  const code = new URL(loginRes.headers.get("location")!).searchParams.get("code")!;
  const exch = await p.handle(
    new Request(`${ORIGIN}/__zeroship/auth/session`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ code }),
    }),
  );
  const token = cookieTokenFrom(exch);
  if (!token) throw new Error("exchange did not set the session cookie");
  return token;
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

  test("passwords default to the well-known dev password, overridable per user", () => {
    // No password → the well-known default.
    assert.equal(parseDevAuthConfig("1")?.passwords["pws_dev00000000000000000"], "dev");
    // Per-user override is preserved; siblings still default.
    const cfg = parseDevAuthConfig(
      JSON.stringify({ users: [{ id: "pws_a", email: "a@x", password: "hunter2" }, { id: "pws_b", email: "b@x" }] }),
    );
    assert.equal(cfg?.passwords["pws_a"], "hunter2");
    assert.equal(cfg?.passwords["pws_b"], "dev");
  });

  test("id-only users get UNIQUE synthesized emails (no collision on the shared default)", () => {
    // The doc's multi-user shape is `{ id, name }` with no email. These must
    // NOT all collapse to dev@localhost, or every user but the first becomes
    // unloginnable and the dropdown is ambiguous.
    const cfg = parseDevAuthConfig(
      JSON.stringify({ users: [{ id: "pws_a", name: "A" }, { id: "pws_b", name: "B" }] }),
    );
    assert.equal(cfg?.users[0].email, "pws_a@localhost");
    assert.equal(cfg?.users[1].email, "pws_b@localhost");
    assert.notEqual(cfg?.users[0].email, cfg?.users[1].email);
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
  test("GET authorize renders a prefilled login form, NOT a frictionless 302", async () => {
    // No auto-login: the developer must see + submit the form. It is prefilled
    // with the dev user's credentials (one click) and carries a CSRF token.
    const p = makeProvider();
    const res = await p.handle(
      new Request("http://localhost:3001/__zeroship/auth/authorize?state=st123&scope=openid"),
    );
    assert.equal(res.status, 200);
    assert.match(res.headers.get("content-type") ?? "", /text\/html/);
    assert.ok(cookieValue(res, "__zeroship_dev_csrf"), "GET authorize sets the CSRF cookie");
    const html = await res.text();
    // A real login form posting back to authorize, with email + password.
    assert.match(html, /method="POST" action="\/__zeroship\/auth\/authorize"/);
    assert.match(html, /name="password"/);
    assert.match(html, /name="csrf"/);
    // Prefilled credentials (one-click) + the original state carried through.
    assert.match(html, /value="dev@localhost"/);
    assert.match(html, /value="dev"/);
    assert.match(html, /name="state" value="st123"/);
    // It is NOT auto-submitted (no inline form.submit()).
    assert.doesNotMatch(html, /\.submit\(\)/);
  });

  test("POST authorize with the prefilled credentials 302s to the callback", async () => {
    const p = makeProvider();
    const res = await devLogin(p, { state: "st123" });
    assert.equal(res.status, 302);
    const loc = new URL(res.headers.get("location")!);
    assert.equal(loc.pathname, "/__zeroship/auth/popup-callback");
    assert.ok(loc.searchParams.get("code"));
    assert.equal(loc.searchParams.get("state"), "st123");
  });

  test("POST authorize with a wrong password re-renders the form with 401 invalid_credentials", async () => {
    const p = makeProvider();
    const res = await devLogin(p, { password: "wrong" });
    assert.equal(res.status, 401);
    assert.match(res.headers.get("content-type") ?? "", /text\/html/);
    const html = await res.text();
    assert.match(html, /invalid email or password/);
    // The form is re-rendered (still usable for a retry).
    assert.match(html, /name="password"/);
  });

  test("a cross-origin redirect_uri is dropped to the same-origin callback (open-redirect guard)", async () => {
    const p = makeProvider();
    const getRes = await p.handle(
      new Request("http://localhost:3001/__zeroship/auth/authorize?state=s"),
    );
    const csrf = cookieValue(getRes, "__zeroship_dev_csrf")!;
    const body = new URLSearchParams({
      csrf,
      state: "s",
      redirect_uri: "https://evil.example/steal", // cross-origin → must be refused
      email: "dev@localhost",
      password: "dev",
    });
    const res = await p.handle(
      new Request("http://localhost:3001/__zeroship/auth/authorize", {
        method: "POST",
        headers: {
          "content-type": "application/x-www-form-urlencoded",
          cookie: `__zeroship_dev_csrf=${csrf}`,
        },
        body: body.toString(),
      }),
    );
    assert.equal(res.status, 302);
    const loc = new URL(res.headers.get("location")!);
    assert.equal(loc.origin, "http://localhost:3001");
    assert.equal(loc.pathname, "/__zeroship/auth/popup-callback");
  });

  test("a same-origin but WRONG-PATH redirect_uri is dropped to the canonical callback (exact-match)", async () => {
    // Origin-only pinning is not enough: a same-origin app path that logs its
    // query could capture the dev code. The match is exact (origin + path).
    const p = makeProvider();
    const getRes = await p.handle(
      new Request("http://localhost:3001/__zeroship/auth/authorize?state=s"),
    );
    const csrf = cookieValue(getRes, "__zeroship_dev_csrf")!;
    const body = new URLSearchParams({
      csrf,
      state: "s",
      redirect_uri: "http://localhost:3001/app/steal-code", // same origin, wrong path
      email: "dev@localhost",
      password: "dev",
    });
    const res = await p.handle(
      new Request("http://localhost:3001/__zeroship/auth/authorize", {
        method: "POST",
        headers: {
          "content-type": "application/x-www-form-urlencoded",
          cookie: `__zeroship_dev_csrf=${csrf}`,
        },
        body: body.toString(),
      }),
    );
    assert.equal(res.status, 302);
    assert.equal(new URL(res.headers.get("location")!).pathname, "/__zeroship/auth/popup-callback");
  });

  test("POST authorize with a mismatched CSRF token is rejected (400)", async () => {
    const p = makeProvider();
    // Submit a form whose CSRF field does not match the cookie.
    const body = new URLSearchParams({
      csrf: "attacker-supplied",
      state: "s",
      email: "dev@localhost",
      password: "dev",
    });
    const res = await p.handle(
      new Request("http://localhost:3001/__zeroship/auth/authorize", {
        method: "POST",
        headers: {
          "content-type": "application/x-www-form-urlencoded",
          cookie: "__zeroship_dev_csrf=the-real-token",
        },
        body: body.toString(),
      }),
    );
    assert.equal(res.status, 400);
    assert.match(await res.text(), /invalid request/);
  });

  test("popup-callback serves the same-origin relay page", async () => {
    const p = makeProvider();
    const res = await p.handle(new Request("http://localhost:3001/__zeroship/auth/popup-callback?code=c&state=s"));
    assert.equal(res.status, 200);
    assert.match(res.headers.get("content-type") ?? "", /text\/html/);
    const html = await res.text();
    // The relay envelope the @zeroship/auth client listens for.
    assert.match(html, /zs:authorization_response/);
    assert.match(html, /tgt\.postMessage\(msg, location\.origin\)/);
  });

  test("popup-callback relay targets opener-OR-parent, pinned to location.origin (never '*')", async () => {
    // Parity with the gateway relay (browser_auth.rs popup_callback_html): the
    // dev relay must reach window.parent for the immersive iframe leg, falling
    // back to window.opener for the popup leg — and NEVER target '*'.
    const p = makeProvider();
    const res = await p.handle(new Request("http://localhost:3001/__zeroship/auth/popup-callback?code=c&state=s"));
    const html = await res.text();
    // Resolves the launcher window: opener (popup) if present-and-distinct,
    // else parent (iframe) if present-and-distinct, else none.
    assert.match(html, /window\.opener && window\.opener !== window/);
    assert.match(html, /window\.parent\s+&& window\.parent\s+!== window/);
    assert.match(html, /tgt\.postMessage\(msg, location\.origin\)/);
    // targetOrigin is pinned to the app origin — a wildcard would leak the code.
    assert.doesNotMatch(html, /postMessage\([^)]*['"]\*['"]/);
  });

  test("exchange mints a session cookie + returns { user, expires_at }", async () => {
    const p = makeProvider();
    // login form → POST credentials → code
    const loginRes = await devLogin(p);
    const code = new URL(loginRes.headers.get("location")!).searchParams.get("code")!;

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
    const token = await devSessionCookie(p);

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

  test("multi-user authorize renders one login form with an email dropdown of users", async () => {
    // >1 dev user → a single login form whose email control is a <select> of
    // the configured users (the default pre-selected), with a script that
    // re-prefills the password on change. No separate picker hop.
    const p = makeProvider(
      JSON.stringify({
        users: [
          { id: "pws_a", email: "a@x", name: "A" },
          { id: "pws_b", email: "b@x", name: "B" },
        ],
      }),
    );
    const res = await p.handle(new Request("http://localhost:3001/__zeroship/auth/authorize?state=s"));
    assert.equal(res.status, 200);
    assert.match(res.headers.get("content-type") ?? "", /text\/html/);
    const html = await res.text();
    assert.match(html, /<select name="email"/);
    assert.match(html, /value="a@x"/);
    assert.match(html, /value="b@x"/);
    // Still one form posting to authorize, with a password + CSRF.
    assert.match(html, /method="POST" action="\/__zeroship\/auth\/authorize"/);
    assert.match(html, /name="password"/);
    assert.match(html, /name="csrf"/);
  });

  test("multi-user: the RENDERED <select> form round-trips (browser submits the default selection)", async () => {
    // Faithful: derive the POST from what the browser actually renders — the
    // `selected` <option>'s email + the prefilled password — not a hand-built
    // body. Proves a browser submitting the real <select> form produces a valid
    // sign-in.
    const p = makeProvider(
      JSON.stringify({
        users: [
          { id: "pws_a", name: "A" },
          { id: "pws_b", name: "B", password: "bee" },
        ],
        defaultUserId: "pws_a",
      }),
    );
    const getRes = await p.handle(new Request(`${ORIGIN}/__zeroship/auth/authorize?state=s`));
    const csrf = cookieValue(getRes, "__zeroship_dev_csrf")!;
    const html = await getRes.text();
    const selectedEmail = /<option value="([^"]*)" selected>/.exec(html)![1];
    const password = /name="password"[^>]*value="([^"]*)"/.exec(html)![1];
    assert.equal(selectedEmail, "pws_a@localhost"); // default pre-selected
    const body = new URLSearchParams({
      csrf,
      state: "s",
      redirect_uri: `${ORIGIN}/__zeroship/auth/popup-callback`,
      email: selectedEmail,
      password,
    });
    const post = await p.handle(
      new Request(`${ORIGIN}/__zeroship/auth/authorize`, {
        method: "POST",
        headers: { "content-type": "application/x-www-form-urlencoded", cookie: `__zeroship_dev_csrf=${csrf}` },
        body: body.toString(),
      }),
    );
    assert.equal(post.status, 302);
    const code = new URL(post.headers.get("location")!).searchParams.get("code")!;
    const exch = await p.handle(
      new Request(`${ORIGIN}/__zeroship/auth/session`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ code }),
      }),
    );
    assert.equal(((await exch.json()) as { user: { id: string } }).user.id, "pws_a");
  });

  test("multi-user: selecting the OTHER user (its mapped password) signs that user in", async () => {
    // The onchange script's email→password map is the source of truth for the
    // re-prefill. Submitting the other user's email + its mapped password must
    // sign THAT user in (per-user password, unique synthesized email).
    const p = makeProvider(
      JSON.stringify({
        users: [
          { id: "pws_a", name: "A" },
          { id: "pws_b", name: "B", password: "bee" },
        ],
      }),
    );
    const getRes = await p.handle(new Request(`${ORIGIN}/__zeroship/auth/authorize?state=s`));
    const csrf = cookieValue(getRes, "__zeroship_dev_csrf")!;
    const html = await getRes.text();
    const map = JSON.parse(/var M=(\{.*?\});/.exec(html)![1]) as Record<string, string>;
    assert.equal(map["pws_b@localhost"], "bee");
    const body = new URLSearchParams({
      csrf,
      state: "s",
      redirect_uri: `${ORIGIN}/__zeroship/auth/popup-callback`,
      email: "pws_b@localhost",
      password: map["pws_b@localhost"],
    });
    const post = await p.handle(
      new Request(`${ORIGIN}/__zeroship/auth/authorize`, {
        method: "POST",
        headers: { "content-type": "application/x-www-form-urlencoded", cookie: `__zeroship_dev_csrf=${csrf}` },
        body: body.toString(),
      }),
    );
    assert.equal(post.status, 302);
    const code = new URL(post.headers.get("location")!).searchParams.get("code")!;
    const exch = await p.handle(
      new Request(`${ORIGIN}/__zeroship/auth/session`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ code }),
      }),
    );
    const u = ((await exch.json()) as { user: { id: string; email: string } }).user;
    assert.equal(u.id, "pws_b");
    assert.equal(u.email, "pws_b@localhost");
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
