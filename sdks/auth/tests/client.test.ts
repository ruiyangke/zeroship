import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { createAuthClient } from "../src/client";
import { AuthError, type AuthChangeEvent, type Session } from "../src/types";
import { APP_ORIGIN, jsonResponse, makeHarness, tokenSuccessBody } from "./harness";

/** Wait until the SDK has persisted a PKCE transaction, then return its state. */
async function awaitTxnState(session: { map: Map<string, string> }): Promise<string> {
  for (let i = 0; i < 50; i++) {
    for (const k of session.map.keys()) {
      if (k.startsWith("@@zsauth@@::txn::")) return k.slice("@@zsauth@@::txn::".length);
    }
    await new Promise((r) => setTimeout(r, 1));
  }
  throw new Error("no PKCE transaction was persisted");
}

/**
 * Wait until the SDK has persisted a transaction AND installed its relay
 * listener (so a dispatched message / BroadcastChannel post is not lost to a
 * race with the async URL build). Returns the flow's `state`.
 */
async function awaitReady(h: {
  session: { map: Map<string, string> };
  window: { messageListenerCount: number };
}): Promise<string> {
  const state = await awaitTxnState(h.session);
  for (let i = 0; i < 50 && h.window.messageListenerCount === 0; i++) {
    await new Promise((r) => setTimeout(r, 1));
  }
  return state;
}

/** All currently-persisted PKCE transaction states. */
function pendingStates(h: { session: { map: Map<string, string> } }): string[] {
  const p = "@@zsauth@@::txn::";
  return [...h.session.map.keys()].filter((k) => k.startsWith(p)).map((k) => k.slice(p.length));
}

/** Wait until at least `n` distinct PKCE transactions AND `n` relay listeners exist. */
async function awaitReadyN(
  h: { session: { map: Map<string, string> }; window: { messageListenerCount: number } },
  n: number,
): Promise<void> {
  for (let i = 0; i < 100; i++) {
    if (pendingStates(h).length >= n && h.window.messageListenerCount >= n) return;
    await new Promise((r) => setTimeout(r, 1));
  }
  throw new Error(`fewer than ${n} concurrent flows became ready`);
}

describe("signInWithOAuth → popup → relay → exchange (faithful end-to-end)", () => {
  test("opens the popup SYNCHRONOUSLY before the async URL build", async () => {
    const h = makeHarness();
    h.fetch.on("/__zs/auth/token", () => jsonResponse(200, tokenSuccessBody()));
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    // Do not await yet — assert the popup exists synchronously after the call.
    const p = client.signInWithOAuth();
    assert.ok(h.window.lastOpened, "window.open must fire synchronously in the gesture");
    // Settle the flow deterministically (close the popup → popup_closed).
    h.window.lastOpened!.close();
    await assert.rejects(p, (e: unknown) => {
      assert.equal((e as AuthError).code, "popup_closed");
      return true;
    });
  });

  test("popup_closed when the user closes the popup before completion", async () => {
    const h = makeHarness();
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    const p = client.signInWithOAuth();
    await awaitReady(h);
    h.window.lastOpened!.close();
    await assert.rejects(p, (e: unknown) => {
      assert.equal((e as AuthError).code, "popup_closed");
      return true;
    });
  });

  test("timeout when neither a relay nor a close arrives within the window", async () => {
    const h = makeHarness();
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    const p = client.signInWithOAuth();
    await awaitReady(h);
    // Never deliver a message and never close — the 300ms test timeout fires.
    await assert.rejects(p, (e: unknown) => {
      assert.equal((e as AuthError).code, "timeout");
      return true;
    });
  });

  test("a spurious early popup.closed does NOT cancel a flow whose code already arrived (COOP)", async () => {
    const h = makeHarness();
    h.fetch.on("/__zs/auth/token", () => jsonResponse(200, tokenSuccessBody()));
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    const p = client.signInWithOAuth();
    const state = await awaitReady(h);
    // Code arrives over the BroadcastChannel fallback (opener severed by COOP)…
    h.env.broadcastChannel!("zs:auth").postMessage({
      type: "zs:authorization_response",
      response: { code: "coop-code", state },
    });
    // …and only THEN does the COOP-severed popup read as closed.
    h.window.lastOpened!.close();
    const session = await p;
    assert.equal(session.access_token, "wrap.access.token", "relay wins the race over a late close");
  });

  test("popup_blocked when window.open returns null", async () => {
    const h = makeHarness();
    h.window.openReturnsNull = true;
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    await assert.rejects(client.signInWithOAuth(), (e: unknown) => {
      assert.ok(e instanceof AuthError);
      assert.equal((e as AuthError).code, "popup_blocked");
      return true;
    });
  });

  test("full handshake: authorize URL, postMessage code, /token exchange, SIGNED_IN", async () => {
    const h = makeHarness();
    h.fetch.on("/__zs/auth/token", () => jsonResponse(200, tokenSuccessBody()));

    const events: AuthChangeEvent[] = [];
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    client.onAuthStateChange((e) => events.push(e));

    const signIn = client.signInWithOAuth({ scopes: ["openid", "profile", "email"] });
    const state = await awaitReady(h);

    // The popup was navigated to GET /__zs/auth/authorize with the S256 params.
    const url = h.window.lastOpened!.location.href;
    assert.ok(url.startsWith(`${APP_ORIGIN}/__zs/auth/authorize?`), url);
    const q = new URL(url).searchParams;
    assert.equal(q.get("code_challenge_method"), "S256");
    assert.ok(q.get("code_challenge"), "code_challenge present");
    assert.equal(q.get("state"), state);
    assert.ok(q.get("nonce"), "nonce present");
    assert.equal(q.get("scope"), "openid profile email");
    assert.equal(
      q.get("redirect_uri"),
      `${APP_ORIGIN}/__zs/auth/popup-callback`,
    );

    // The popup-callback relays the code+state via postMessage to the opener.
    h.window.dispatchMessage({
      origin: APP_ORIGIN,
      data: {
        type: "zs:authorization_response",
        response: { code: "the-code", state },
      },
    });

    const session = await signIn;
    assert.equal(session.access_token, "wrap.access.token");
    assert.equal(session.user.id, "pws_alice");
    assert.equal(session.user.emailVerified, true);
    assert.deepEqual(session.scopes, ["openid", "profile", "email"]);

    // /token was posted with X-ZS-Auth + the PKCE verifier + grant_type.
    const tokenReq = h.fetch.requests.find((r) => r.url.includes("/__zs/auth/token"))!;
    assert.equal(tokenReq.method, "POST");
    assert.equal(tokenReq.headers["x-zs-auth"], "1");
    const body = tokenReq.body as Record<string, string>;
    assert.equal(body.grant_type, "authorization_code");
    assert.equal(body.code, "the-code");
    assert.ok(body.code_verifier, "code_verifier sent from sessionStorage");
    assert.equal(body.redirect_uri, `${APP_ORIGIN}/__zs/auth/popup-callback`);

    assert.deepEqual(events, ["SIGNED_IN"]);
    // The breadcrumb fast-path mirror was written.
    assert.match(h.cookies.get(), /is\.authenticated=true/);
    // The transaction was cleared on completion.
    assert.equal(h.session.map.size, 0, "spent PKCE transaction must be cleared");
  });

  test("prompt is threaded to the authorize URL for step-up re-auth", async () => {
    const h = makeHarness();
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    const signIn = client.signInWithOAuth({ prompt: "login" });
    await awaitReady(h);
    const q = new URL(h.window.lastOpened!.location.href).searchParams;
    assert.equal(q.get("prompt"), "login", "explicit prompt must reach GET /authorize");
    // Settle the flow.
    h.window.lastOpened!.close();
    await signIn.catch(() => {});
  });

  test("no prompt param when none requested (Hydra SSO skip fires)", async () => {
    const h = makeHarness();
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    const signIn = client.signInWithOAuth();
    await awaitReady(h);
    const q = new URL(h.window.lastOpened!.location.href).searchParams;
    assert.equal(q.has("prompt"), false, "omitted by default so SSO can skip");
    assert.equal(q.has("idp_hint"), false, "no provider ⇒ no idp_hint");
    h.window.lastOpened!.close();
    await signIn.catch(() => {});
  });

  test("provider is threaded to the authorize URL as idp_hint (Fix 5)", async () => {
    const h = makeHarness();
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    const signIn = client.signInWithOAuth({ provider: "github" });
    await awaitReady(h);
    const q = new URL(h.window.lastOpened!.location.href).searchParams;
    assert.equal(q.get("idp_hint"), "github", "provider must reach GET /authorize as idp_hint");
    h.window.lastOpened!.close();
    await signIn.catch(() => {});
  });

  test("signInWithPassword threads provider=password as idp_hint (Fix 5)", async () => {
    const h = makeHarness();
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    const signIn = client.signInWithPassword();
    await awaitReady(h);
    const q = new URL(h.window.lastOpened!.location.href).searchParams;
    assert.equal(q.get("idp_hint"), "password", "signInWithPassword routes to the password IdP");
    h.window.lastOpened!.close();
    await signIn.catch(() => {});
  });

  test("requestScopes steps up with prompt=consent so new scopes are granted", async () => {
    const h = makeHarness();
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    const signIn = client.requestScopes(["offline_access"]);
    await awaitReady(h);
    const q = new URL(h.window.lastOpened!.location.href).searchParams;
    assert.equal(q.get("prompt"), "consent", "requestScopes must re-show consent");
    assert.match(q.get("scope") ?? "", /offline_access/, "the requested scope is unioned in");
    h.window.lastOpened!.close();
    await signIn.catch(() => {});
  });

  test("getAccessTokenWithPopup runs an interactive consent step-up and returns the minted token", async () => {
    const h = makeHarness();
    h.fetch.on("/__zs/auth/token", () => jsonResponse(200, tokenSuccessBody({ scope: "openid profile email payments:write" })));
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);

    const tokenP = client.getAccessTokenWithPopup({ scopes: ["payments:write"] });
    const state = await awaitReady(h);

    // It is the INTERACTIVE variant: a popup must have opened with prompt=consent
    // and the requested scope unioned in (no silent mint).
    const q = new URL(h.window.lastOpened!.location.href).searchParams;
    assert.equal(q.get("prompt"), "consent", "getAccessTokenWithPopup must drive an interactive consent step-up");
    assert.match(q.get("scope") ?? "", /payments:write/, "the requested scope is unioned in");

    h.window.dispatchMessage({
      origin: APP_ORIGIN,
      data: { type: "zs:authorization_response", response: { code: "stepup-code", state } },
    });

    const token = await tokenP;
    assert.equal(token, "wrap.access.token", "resolves the freshly-minted access token, not a Session");
  });

  test("a relay message for a DIFFERENT flow's state is IGNORED (no cross-flow delivery)", async () => {
    // MAJOR fix: the relay channels are origin-shared, so a well-formed
    // response for a CONCURRENT flow (wrong `state`) can arrive. It must be
    // IGNORED (the flow keeps waiting), NOT delivered to this flow's exchange.
    // Here the foreign-state message is dropped and the flow then ends via the
    // popup-close hint (popup_closed) — proving the stray code never reached
    // /token and never settled this flow with the wrong code.
    const h = makeHarness();
    let tokenCalls = 0;
    h.fetch.on("/__zs/auth/token", () => {
      tokenCalls++;
      return jsonResponse(200, tokenSuccessBody());
    });
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    const signIn = client.signInWithOAuth();
    await awaitReady(h);
    // A code for SOME OTHER flow (state the SDK never minted) — must be ignored.
    h.window.dispatchMessage({
      origin: APP_ORIGIN,
      data: { type: "zs:authorization_response", response: { code: "foreign", state: "WRONG" } },
    });
    // The flow did NOT settle on the foreign message — close the popup so it
    // terminates via the close hint instead.
    h.window.lastOpened!.close();
    await assert.rejects(signIn, (e: unknown) => {
      assert.equal((e as AuthError).code, "popup_closed");
      return true;
    });
    assert.equal(tokenCalls, 0, "a foreign-state code must never reach /token");
  });

  test("concurrent flows: each completes with its OWN code (relay state filtering)", async () => {
    // Two interleaved sign-ins on the same origin. Each flow's popup relays a
    // code tagged with ITS state; the SDK must route each code to its own flow.
    const h = makeHarness();
    h.fetch.on("/__zs/auth/token", (req) =>
      jsonResponse(200, tokenSuccessBody({ access_token: `tok-${(req.body as { code: string }).code}` })),
    );
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);

    const signInA = client.signInWithOAuth();
    const stateA = await awaitReady(h);
    const signInB = client.signInWithOAuth();
    // Wait until BOTH flows have persisted a txn and installed their relay
    // listeners, then recover the second flow's distinct state.
    await awaitReadyN(h, 2);
    const stateB = pendingStates(h).find((s) => s !== stateA)!;
    assert.ok(stateB, "two distinct pending PKCE states exist");

    // Relay B's code FIRST (out of order) — flow A must ignore it.
    h.window.dispatchMessage({
      origin: APP_ORIGIN,
      data: { type: "zs:authorization_response", response: { code: "code-B", state: stateB } },
    });
    h.window.dispatchMessage({
      origin: APP_ORIGIN,
      data: { type: "zs:authorization_response", response: { code: "code-A", state: stateA } },
    });

    const [a, b] = await Promise.all([signInA, signInB]);
    assert.equal(a.access_token, "tok-code-A", "flow A resolved with A's code");
    assert.equal(b.access_token, "tok-code-B", "flow B resolved with B's code");
  });

  test("a relay error response maps to a typed AuthError", async () => {
    const h = makeHarness();
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    const signIn = client.signInWithOAuth();
    const state = await awaitReady(h);
    h.window.dispatchMessage({
      origin: APP_ORIGIN,
      data: {
        type: "zs:authorization_response",
        response: { error: "consent_required", error_description: "needs consent", state },
      },
    });
    await assert.rejects(signIn, (e: unknown) => {
      assert.equal((e as AuthError).code, "consent_required");
      return true;
    });
  });
});

describe("exchangeCodeForSession", () => {
  test("recovers the verifier from sessionStorage and posts the gateway /token shape", async () => {
    const h = makeHarness();
    h.fetch.on("/__zs/auth/token", () => jsonResponse(200, tokenSuccessBody()));
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);

    // Begin a flow to seed a transaction, but resolve it via the redirect-style
    // exchange (no popup message) to prove the sessionStorage recovery path.
    const signIn = client.signInWithOAuth();
    const state = await awaitReady(h);

    const session = await client.exchangeCodeForSession("redirect-code", state);
    assert.equal(session.access_token, "wrap.access.token");
    const req = h.fetch.requests.find((r) => r.url.includes("/__zs/auth/token"))!;
    assert.equal((req.body as Record<string, string>).code, "redirect-code");

    // The original popup promise loses the race; close the popup to settle it.
    h.window.lastOpened!.close();
    await signIn.catch(() => {});
  });

  test("missing_code_verifier when no transaction matches", async () => {
    const h = makeHarness();
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    await assert.rejects(client.exchangeCodeForSession("orphan-code", "nope"), (e: unknown) => {
      assert.equal((e as AuthError).code, "missing_code_verifier");
      return true;
    });
  });
});

describe("getSession / getUser / isAuthenticated / hasScope", () => {
  async function signedInClient() {
    const h = makeHarness();
    h.fetch.on("/__zs/auth/token", () => jsonResponse(200, tokenSuccessBody()));
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    const signIn = client.signInWithOAuth();
    const state = await awaitReady(h);
    h.window.dispatchMessage({
      origin: APP_ORIGIN,
      data: { type: "zs:authorization_response", response: { code: "c", state } },
    });
    await signIn;
    return { h, client };
  }

  test("getSession is cache-only (no network) and returns the cached session", async () => {
    const { h, client } = await signedInClient();
    const before = h.fetch.requests.length;
    const s = await client.getSession();
    assert.equal(s?.access_token, "wrap.access.token");
    assert.equal(h.fetch.requests.length, before, "getSession must not hit the network");
    assert.equal(client.isAuthenticated(), true);
    assert.equal(client.hasScope("profile"), true);
    assert.equal(client.hasScope("admin"), false);
  });

  test("getUser ALWAYS probes GET /__zs/auth/session and updates the cache", async () => {
    const { h, client } = await signedInClient();
    h.fetch.on(
      (u) => u.includes("/__zs/auth/session") && !u.includes("mint=1"),
      () =>
        jsonResponse(200, {
          user: {
            id: "pws_alice",
            email: "alice@relay.zeroship.ai",
            email_verified: true,
            name: "Alice Renamed",
            avatar: null,
            scopes: ["openid", "profile", "email"],
          },
        }),
    );
    const events: AuthChangeEvent[] = [];
    client.onAuthStateChange((e) => events.push(e));
    const user = await client.getUser();
    assert.equal(user?.name, "Alice Renamed");
    const probed = h.fetch.requests.some(
      (r) => r.url.includes("/__zs/auth/session") && !r.url.includes("mint=1"),
    );
    assert.ok(probed, "getUser must probe /session");
    assert.deepEqual(events, ["USER_UPDATED"]);
  });

  test("getUser → 401 login_required clears the breadcrumb and signs out", async () => {
    const { h, client } = await signedInClient();
    h.fetch.on(
      (u) => u.includes("/__zs/auth/session") && !u.includes("mint=1"),
      () => jsonResponse(401, { error: "login_required" }),
    );
    const events: AuthChangeEvent[] = [];
    client.onAuthStateChange((e) => events.push(e));
    const user = await client.getUser();
    assert.equal(user, null);
    assert.equal(client.isAuthenticated(), false);
    assert.doesNotMatch(h.cookies.get(), /is\.authenticated=true/, "breadcrumb cleared");
    assert.deepEqual(events, ["SIGNED_OUT"]);
  });
});

describe("signOut", () => {
  test("POSTs /__zs/auth/signout with X-ZS-Auth + scope, clears cache + breadcrumb, emits SIGNED_OUT", async () => {
    const h = makeHarness();
    h.fetch.on("/__zs/auth/token", () => jsonResponse(200, tokenSuccessBody()));
    h.fetch.on("/__zs/auth/signout", () => jsonResponse(204, null));
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);

    const signIn = client.signInWithOAuth();
    const state = await awaitReady(h);
    h.window.dispatchMessage({
      origin: APP_ORIGIN,
      data: { type: "zs:authorization_response", response: { code: "c", state } },
    });
    await signIn;

    const events: AuthChangeEvent[] = [];
    client.onAuthStateChange((e) => events.push(e));
    await client.signOut({ scope: "global" });

    const req = h.fetch.requests.find((r) => r.url.includes("/__zs/auth/signout"))!;
    assert.equal(req.method, "POST");
    assert.equal(req.headers["x-zs-auth"], "1");
    assert.equal((req.body as Record<string, string>).scope, "global");
    assert.equal(client.isAuthenticated(), false);
    assert.doesNotMatch(h.cookies.get(), /is\.authenticated=true/);
    assert.deepEqual(events, ["SIGNED_OUT"]);
  });

  test("clears local state even when the network leg fails (idempotent intent)", async () => {
    const h = makeHarness();
    h.fetch.on("/__zs/auth/token", () => jsonResponse(200, tokenSuccessBody()));
    h.fetch.on("/__zs/auth/signout", () => {
      throw new Error("network down");
    });
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    const signIn = client.signInWithOAuth();
    const state = await awaitReady(h);
    h.window.dispatchMessage({
      origin: APP_ORIGIN,
      data: { type: "zs:authorization_response", response: { code: "c", state } },
    });
    await signIn;

    await client.signOut();
    assert.equal(client.isAuthenticated(), false, "local sign-out must complete on network failure");
  });
});
