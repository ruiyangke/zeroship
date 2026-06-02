import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { createAuthClient } from "../src/client";
import { AuthError, type AuthChangeEvent, type Session } from "../src/types";
import { APP_ORIGIN, jsonResponse, makeHarness, tokenSuccessBody, SESSION_EXCHANGE } from "./harness";

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

/**
 * Wait until the SDK has created the login iframe AND installed its relay
 * listener (the iframe analogue of {@link awaitReady}). Returns the flow's
 * `state` (recovered from the persisted PKCE transaction).
 */
async function awaitIframeReady(h: {
  session: { map: Map<string, string> };
  window: { messageListenerCount: number };
  lastIframe?: { src: string };
}): Promise<string> {
  const state = await awaitTxnState(h.session);
  for (let i = 0; i < 50 && (!h.lastIframe || h.window.messageListenerCount === 0); i++) {
    await new Promise((r) => setTimeout(r, 1));
  }
  return state;
}

const AUTH_ORIGIN_SAME_SITE = "https://auth.zeroship.ai"; // eTLD+1 == zeroship.ai (== APP_ORIGIN's)
const AUTH_ORIGIN_CROSS_SITE = "https://auth.example.com"; // different eTLD+1 ⇒ popup fallback

describe("signInWithOAuth → popup → relay → exchange (faithful end-to-end)", () => {
  test("opens the popup SYNCHRONOUSLY before the async URL build", async () => {
    const h = makeHarness();
    h.fetch.on(SESSION_EXCHANGE, () => jsonResponse(200, tokenSuccessBody()));
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
    h.fetch.on(SESSION_EXCHANGE, () => jsonResponse(200, tokenSuccessBody()));
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
    // BFF: no client-held token (the cookie is the credential). The exchange
    // still resolves a Session — assert the identity projection.
    assert.equal(session.user.id, "pws_alice", "relay wins the race over a late close");
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

  test("full handshake: authorize URL, postMessage code, POST /session exchange, SIGNED_IN", async () => {
    const h = makeHarness();
    h.fetch.on(SESSION_EXCHANGE, () => jsonResponse(200, tokenSuccessBody()));

    const events: AuthChangeEvent[] = [];
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    client.onAuthStateChange((e) => events.push(e));

    const signIn = client.signInWithOAuth({ scopes: ["openid", "profile", "email"] });
    const state = await awaitReady(h);

    // The popup was navigated to GET /__zeroship/auth/authorize with the S256 params.
    const url = h.window.lastOpened!.location.href;
    assert.ok(url.startsWith(`${APP_ORIGIN}/__zeroship/auth/authorize?`), url);
    const q = new URL(url).searchParams;
    assert.equal(q.get("code_challenge_method"), "S256");
    assert.ok(q.get("code_challenge"), "code_challenge present");
    assert.equal(q.get("state"), state);
    assert.ok(q.get("nonce"), "nonce present");
    assert.equal(q.get("scope"), "openid profile email");
    assert.equal(
      q.get("redirect_uri"),
      `${APP_ORIGIN}/__zeroship/auth/popup-callback`,
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
    assert.equal(session.user.id, "pws_alice");
    assert.equal(session.user.emailVerified, true);
    assert.deepEqual(session.scopes, ["openid", "profile", "email"]);

    // POST /__zeroship/auth/session was posted with X-ZS-Auth + the PKCE verifier + grant_type.
    const tokenReq = h.fetch.requests.find(
      (r) => r.method === "POST" && r.url.includes("/__zeroship/auth/session"),
    )!;
    assert.equal(tokenReq.method, "POST");
    assert.equal(tokenReq.headers["x-zs-auth"], "1");
    const body = tokenReq.body as Record<string, string>;
    assert.equal(body.grant_type, "authorization_code");
    assert.equal(body.code, "the-code");
    assert.ok(body.code_verifier, "code_verifier sent from sessionStorage");
    assert.equal(body.redirect_uri, `${APP_ORIGIN}/__zeroship/auth/popup-callback`);

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

  test("requestScopes runs an interactive consent step-up and resolves an identity-only session (NO token)", async () => {
    const h = makeHarness();
    h.fetch.on(
      SESSION_EXCHANGE,
      () =>
        jsonResponse(
          200,
          tokenSuccessBody({
            user: {
              id: "pws_alice",
              email: "alice@relay.zeroship.ai",
              email_verified: true,
              name: "Alice",
              avatar: null,
              scopes: ["openid", "profile", "email", "payments:write"],
            },
          }),
        ),
    );
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);

    const stepUp = client.requestScopes(["payments:write"]);
    const state = await awaitReady(h);

    // It is the INTERACTIVE step-up: a popup must have opened with prompt=consent
    // and the requested scope unioned in (no silent mint).
    const q = new URL(h.window.lastOpened!.location.href).searchParams;
    assert.equal(q.get("prompt"), "consent", "requestScopes must drive an interactive consent step-up");
    assert.match(q.get("scope") ?? "", /payments:write/, "the requested scope is unioned in");

    h.window.dispatchMessage({
      origin: APP_ORIGIN,
      data: { type: "zs:authorization_response", response: { code: "stepup-code", state } },
    });

    const session = await stepUp;
    // BFF model: the step-up's effect is a server-side grant + a re-signed
    // session cookie; the browser receives identity ONLY, never a token.
    assert.equal(
      "access_token" in (session as unknown as Record<string, unknown>),
      false,
      "BFF model: no browser-held token after step-up",
    );
    assert.ok(session.scopes.includes("payments:write"), "the newly consented scope is reflected");
  });

  test("a relay message for a DIFFERENT flow's state is IGNORED (no cross-flow delivery)", async () => {
    // MAJOR fix: the relay channels are origin-shared, so a well-formed
    // response for a CONCURRENT flow (wrong `state`) can arrive. It must be
    // IGNORED (the flow keeps waiting), NOT delivered to this flow's exchange.
    // Here the foreign-state message is dropped and the flow then ends via the
    // popup-close hint (popup_closed) — proving the stray code never reached
    // POST /session and never settled this flow with the wrong code.
    const h = makeHarness();
    let tokenCalls = 0;
    h.fetch.on(SESSION_EXCHANGE, () => {
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
    assert.equal(tokenCalls, 0, "a foreign-state code must never reach POST /session");
  });

  test("concurrent flows: each completes with its OWN code (relay state filtering)", async () => {
    // Two interleaved sign-ins on the same origin. Each flow's popup relays a
    // code tagged with ITS state; the SDK must route each code to its own flow.
    const h = makeHarness();
    // BFF: the response carries no token, so tag each flow's identity by code
    // (a distinct user.id) to prove each code routed to its own flow.
    h.fetch.on(SESSION_EXCHANGE, (req) =>
      jsonResponse(
        200,
        tokenSuccessBody({
          user: {
            id: `pws_${(req.body as { code: string }).code}`,
            email: "alice@relay.zeroship.ai",
            email_verified: true,
            name: "Alice",
            avatar: null,
            scopes: ["openid", "profile", "email"],
          },
        }),
      ),
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
    assert.equal(a.user.id, "pws_code-A", "flow A resolved with A's code");
    assert.equal(b.user.id, "pws_code-B", "flow B resolved with B's code");
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

describe("signInWithOAuth({provider:'password'}) → immersive iframe (same-site console)", () => {
  test("opens the iframe (NOT a popup), sets src to the authorize URL, relays a code, tears down", async () => {
    const h = makeHarness();
    h.fetch.on(SESSION_EXCHANGE, () => jsonResponse(200, tokenSuccessBody()));
    const events: AuthChangeEvent[] = [];
    const client = createAuthClient(
      { appOrigin: APP_ORIGIN, immersive: true, authOrigin: AUTH_ORIGIN_SAME_SITE },
      h.env,
    );
    client.onAuthStateChange((e) => events.push(e));

    const signIn = client.signInWithOAuth({ provider: "password" });
    const state = await awaitIframeReady(h);

    // The iframe was created via the injected factory — NOT window.open.
    assert.ok(h.lastIframe, "the immersive path must create an iframe via the injected handle");
    assert.equal(h.window.lastOpened, undefined, "the immersive path must NOT open a popup window");

    // The iframe `src` (the ELEMENT attribute) is the authorize URL with the
    // S256 PKCE params + the app-origin callback redirect_uri + idp_hint=password.
    const src = h.lastIframe!.src;
    assert.ok(src.startsWith(`${APP_ORIGIN}/__zeroship/auth/authorize?`), src);
    const q = new URL(src).searchParams;
    assert.equal(q.get("code_challenge_method"), "S256");
    assert.ok(q.get("code_challenge"), "code_challenge present");
    assert.equal(q.get("state"), state);
    assert.equal(q.get("idp_hint"), "password", "the first-party password UI is selected");
    assert.equal(q.get("redirect_uri"), `${APP_ORIGIN}/__zeroship/auth/popup-callback`);
    // The driver NEVER re-assigns src (it would mean touching the live frame).
    assert.equal(h.lastIframe!.srcReassignments, 0, "src is set once on the element, never re-navigated");

    // The app-origin callback posts {code,state} to window.parent (the console top).
    h.window.dispatchMessage({
      origin: APP_ORIGIN,
      data: { type: "zs:authorization_response", response: { code: "iframe-code", state } },
    });

    const session = await signIn;
    assert.equal(session.user.id, "pws_alice", "the relayed code drove POST /session → SIGNED_IN");
    assert.deepEqual(events, ["SIGNED_IN"]);
    assert.equal(h.lastIframe!.removed, true, "the iframe is torn down on settle");

    // The surviving code-exchange path drove POST /session (no /password call).
    const exchanged = h.fetch.requests.find(
      (r) => r.method === "POST" && r.url.includes("/__zeroship/auth/session"),
    );
    assert.ok(exchanged, "the iframe drives the surviving POST /session exchange");
    assert.equal((exchanged!.body as Record<string, string>).code, "iframe-code");
    assert.ok(
      !h.fetch.requests.some((r) => r.url.includes("/__zeroship/auth/password")),
      "no /password endpoint is ever called (it is deleted)",
    );
  });

  test("falls back to the POPUP when immersive is false (default)", async () => {
    const h = makeHarness();
    const client = createAuthClient(
      { appOrigin: APP_ORIGIN, authOrigin: AUTH_ORIGIN_SAME_SITE /* immersive omitted ⇒ false */ },
      h.env,
    );
    const signIn = client.signInWithOAuth({ provider: "password" });
    await awaitReady(h);
    assert.ok(h.window.lastOpened, "immersive:false ⇒ the popup window opens");
    assert.equal(h.lastIframe, undefined, "no iframe is created when immersive is off");
    // The popup was navigated to the authorize URL with idp_hint=password.
    const q = new URL(h.window.lastOpened!.location.href).searchParams;
    assert.equal(q.get("idp_hint"), "password");
    h.window.lastOpened!.close();
    await signIn.catch(() => {});
  });

  test("falls back to the POPUP when authOrigin is cross-site (custom-domain console)", async () => {
    const h = makeHarness();
    const client = createAuthClient(
      { appOrigin: APP_ORIGIN, immersive: true, authOrigin: AUTH_ORIGIN_CROSS_SITE },
      h.env,
    );
    const signIn = client.signInWithOAuth({ provider: "password" });
    await awaitReady(h);
    assert.ok(h.window.lastOpened, "cross-site authOrigin ⇒ the popup window opens");
    assert.equal(h.lastIframe, undefined, "no iframe is created when the surface is cross-site");
    h.window.lastOpened!.close();
    await signIn.catch(() => {});
  });

  test("federated providers stay a POPUP even with immersive enabled", async () => {
    const h = makeHarness();
    const client = createAuthClient(
      { appOrigin: APP_ORIGIN, immersive: true, authOrigin: AUTH_ORIGIN_SAME_SITE },
      h.env,
    );
    const signIn = client.signInWithOAuth({ provider: "google" });
    await awaitReady(h);
    assert.ok(h.window.lastOpened, "google is federated ⇒ popup, never the iframe");
    assert.equal(h.lastIframe, undefined, "no iframe for a federated provider");
    const q = new URL(h.window.lastOpened!.location.href).searchParams;
    assert.equal(q.get("idp_hint"), "google");
    h.window.lastOpened!.close();
    await signIn.catch(() => {});
  });

  test("falls back to the POPUP when the env has no iframe factory (no DOM)", async () => {
    const h = makeHarness({ iframe: false });
    const client = createAuthClient(
      { appOrigin: APP_ORIGIN, immersive: true, authOrigin: AUTH_ORIGIN_SAME_SITE },
      h.env,
    );
    const signIn = client.signInWithOAuth({ provider: "password" });
    await awaitReady(h);
    assert.ok(h.window.lastOpened, "no iframe factory ⇒ graceful popup fallback");
    assert.equal(h.lastIframe, undefined);
    // Exactly one pending transaction: the rolled-back iframe txn must not leak.
    assert.equal(pendingStates(h).length, 1, "the abandoned iframe transaction is rolled back");
    h.window.lastOpened!.close();
    await signIn.catch(() => {});
  });

  test("the modal close affordance cancels the iframe flow (popup_closed) and tears it down", async () => {
    const h = makeHarness();
    const client = createAuthClient(
      { appOrigin: APP_ORIGIN, immersive: true, authOrigin: AUTH_ORIGIN_SAME_SITE },
      h.env,
    );
    const signIn = client.signInWithOAuth({ provider: "password" });
    await awaitIframeReady(h);
    assert.ok(h.lastIframe);
    // User closes the modal → the injected iframe's cancel signal fires.
    h.lastIframe!.cancel();
    await assert.rejects(signIn, (e: unknown) => {
      assert.equal((e as AuthError).code, "popup_closed");
      return true;
    });
    assert.equal(h.lastIframe!.removed, true, "the iframe is torn down on cancel");
  });

  test("a relay error inside the iframe maps to a typed AuthError (e.g. invalid_credentials)", async () => {
    const h = makeHarness();
    const client = createAuthClient(
      { appOrigin: APP_ORIGIN, immersive: true, authOrigin: AUTH_ORIGIN_SAME_SITE },
      h.env,
    );
    const signIn = client.signInWithOAuth({ provider: "password" });
    const state = await awaitIframeReady(h);
    // The framed /login surfaces a rejected password via the relay error envelope.
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
    assert.equal(h.lastIframe!.removed, true, "the iframe is torn down on a relay error");
  });
});

describe("exchangeCodeForSession", () => {
  test("recovers the verifier from sessionStorage and posts the gateway /session shape", async () => {
    const h = makeHarness();
    h.fetch.on(SESSION_EXCHANGE, () => jsonResponse(200, tokenSuccessBody()));
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);

    // Begin a flow to seed a transaction, but resolve it via the redirect-style
    // exchange (no popup message) to prove the sessionStorage recovery path.
    const signIn = client.signInWithOAuth();
    const state = await awaitReady(h);

    const session = await client.exchangeCodeForSession("redirect-code", state);
    assert.equal(session.user.id, "pws_alice");
    const req = h.fetch.requests.find(
      (r) => r.method === "POST" && r.url.includes("/__zeroship/auth/session"),
    )!;
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
    h.fetch.on(SESSION_EXCHANGE, () => jsonResponse(200, tokenSuccessBody()));
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
    assert.equal(s?.user.id, "pws_alice");
    assert.equal(h.fetch.requests.length, before, "getSession must not hit the network");
    assert.equal(client.isAuthenticated(), true);
    assert.equal(client.hasScope("profile"), true);
    assert.equal(client.hasScope("admin"), false);
  });

  test("getUser ALWAYS probes GET /__zeroship/auth/session and updates the cache", async () => {
    const { h, client } = await signedInClient();
    h.fetch.on(
      (u) => u.includes("/__zeroship/auth/session") && !u.includes("mint=1"),
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
      (r) => r.url.includes("/__zeroship/auth/session") && !r.url.includes("mint=1"),
    );
    assert.ok(probed, "getUser must probe /session");
    assert.deepEqual(events, ["USER_UPDATED"]);
  });

  test("getUser → 401 login_required clears the breadcrumb and signs out", async () => {
    const { h, client } = await signedInClient();
    h.fetch.on(
      (u) => u.includes("/__zeroship/auth/session") && !u.includes("mint=1"),
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
  test("POSTs /__zeroship/auth/signout with X-ZS-Auth + scope, clears cache + breadcrumb, emits SIGNED_OUT", async () => {
    const h = makeHarness();
    h.fetch.on(SESSION_EXCHANGE, () => jsonResponse(200, tokenSuccessBody()));
    h.fetch.on("/__zeroship/auth/signout", () => jsonResponse(204, null));
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

    const req = h.fetch.requests.find((r) => r.url.includes("/__zeroship/auth/signout"))!;
    assert.equal(req.method, "POST");
    assert.equal(req.headers["x-zs-auth"], "1");
    assert.equal((req.body as Record<string, string>).scope, "global");
    assert.equal(client.isAuthenticated(), false);
    assert.doesNotMatch(h.cookies.get(), /is\.authenticated=true/);
    assert.deepEqual(events, ["SIGNED_OUT"]);
  });

  test("clears local state even when the network leg fails (idempotent intent)", async () => {
    const h = makeHarness();
    h.fetch.on(SESSION_EXCHANGE, () => jsonResponse(200, tokenSuccessBody()));
    h.fetch.on("/__zeroship/auth/signout", () => {
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
