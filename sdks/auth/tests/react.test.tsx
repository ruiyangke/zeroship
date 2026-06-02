/**
 * Tests for `@zeroship/auth/react` — the React adapter (Slice 2b).
 *
 * Strategy (faithful): we render the REAL `<AuthProvider>` / `useAuth` /
 * `<SignInButton>` / `<SignOutButton>` against a HAND-ROLLED fake `AuthClient`
 * with a REAL `onAuthStateChange` emitter (the same Set-of-listeners contract
 * the headless client implements). Nothing in the provider logic under test is
 * stubbed — we drive it exactly as a browser would:
 *
 *   - the fake's methods record their call order/timing so we can assert
 *     `signInWithOAuth` fires SYNCHRONOUSLY in the click handler (gesture-safe);
 *   - `emit(event, session)` is what a real exchange / signout would call, so
 *     asserting on the rendered snapshot exercises the subscription path;
 *   - the redirect-callback mount path runs against a real `window.location`
 *     (happy-dom) and a real `history.replaceState`.
 *
 * DOM globals come from `tests/setup-dom.ts` (happy-dom global registrator),
 * loaded via `--import` before this module.
 */

import { test, describe, beforeEach, afterEach } from "node:test";
import assert from "node:assert/strict";
import * as React from "react";
import { act, render, cleanup, fireEvent, screen, waitFor } from "@testing-library/react";

import {
  AuthModal,
  AuthProvider,
  SignIn,
  SignInButton,
  SignOutButton,
  SignedIn,
  SignedOut,
  useAuth,
  type AuthContextValue,
} from "../src/react.js";
import { AuthError, type AuthChangeEvent, type Session, type User } from "../src/types.js";
import { createAuthClient, type AuthClient } from "../src/client.js";
import { makeHarness, jsonResponse } from "./harness.js";

// ── fixtures ─────────────────────────────────────────────────────────────────

const USER: User = {
  id: "pws_alice",
  email: "alice@relay.zeroship.test",
  emailVerified: true,
  name: "Alice",
  avatar: null,
  scopes: ["openid", "profile", "email"],
};

function makeSession(over: Partial<Session> = {}): Session {
  return {
    expires_at: Math.floor(Date.now() / 1000) + 3600,
    user: USER,
    scopes: ["openid", "profile", "email"],
    ...over,
  };
}

// ── fake AuthClient (real emitter) ───────────────────────────────────────────

type Listener = (event: AuthChangeEvent, session: Session | null) => void;

interface CallLog {
  signInWithOAuth: unknown[][];
  signOut: unknown[][];
  checkSession: number;
  exchangeCodeForSession: Array<[string, string | undefined]>;
  requestScopes: unknown[][];
}

interface FakeClient extends AuthClient {
  readonly calls: CallLog;
  /** Drive a state transition exactly as a real exchange/signout would. */
  emit(event: AuthChangeEvent, session: Session | null): void;
  /** Number of live `onAuthStateChange` subscribers. */
  listenerCount(): number;
  /** Override what `checkSession` resolves to (default: null → anonymous). */
  checkSessionResult: Session | null;
  /** When set, `signInWithOAuth` rejects with this instead of resolving. */
  signInError: AuthError | null;
  /** When set, `exchangeCodeForSession` rejects with this. */
  exchangeError: AuthError | null;
  /** When set, `checkSession` (the mount recovery probe) rejects with this. */
  checkSessionReject: AuthError | null;
  /**
   * When set, `signInWithOAuth({provider:'password'})` resolves a SIGNED_IN
   * session (mirrors the real iframe flow's success). Default false ⇒ it just
   * records the call and resolves a session without emitting.
   */
  oauthEmitsSignedIn: boolean;
}

function makeFakeClient(): FakeClient {
  const listeners = new Set<Listener>();
  const calls: CallLog = {
    signInWithOAuth: [],
    signOut: [],
    checkSession: 0,
    exchangeCodeForSession: [],
    requestScopes: [],
  };
  let current: Session | null = null;

  const client: FakeClient = {
    calls,
    checkSessionResult: null,
    signInError: null,
    exchangeError: null,
    checkSessionReject: null,
    oauthEmitsSignedIn: false,

    emit(event, session) {
      current = session;
      for (const cb of listeners) cb(event, session);
    },
    listenerCount: () => listeners.size,

    onAuthStateChange(cb) {
      listeners.add(cb);
      return {
        unsubscribe() {
          listeners.delete(cb);
        },
      };
    },

    async signInWithOAuth(opts) {
      calls.signInWithOAuth.push([opts]);
      if (client.signInError) throw client.signInError;
      const s = makeSession();
      // A real iframe/popup flow emits SIGNED_IN as it stores the session.
      if (client.oauthEmitsSignedIn) client.emit("SIGNED_IN", s);
      return s;
    },
    async exchangeCodeForSession(code, state) {
      calls.exchangeCodeForSession.push([code, state]);
      if (client.exchangeError) throw client.exchangeError;
      const s = makeSession();
      // A real exchange emits SIGNED_IN as it stores the session.
      client.emit("SIGNED_IN", s);
      return s;
    },
    async checkSession() {
      calls.checkSession += 1;
      if (client.checkSessionReject) throw client.checkSessionReject;
      if (client.checkSessionResult) client.emit("SIGNED_IN", client.checkSessionResult);
      return client.checkSessionResult;
    },

    async getSession() {
      return current;
    },
    async getUser() {
      return current?.user ?? null;
    },
    async refreshSession() {
      const s = makeSession();
      client.emit("SESSION_REFRESHED", s);
      return s;
    },
    isAuthenticated() {
      return current != null;
    },
    hasScope(scope) {
      return current?.scopes.includes(scope) ?? false;
    },
    async requestScopes(scopes) {
      // Interactive step-up: a real client opens a popup and emits SIGNED_IN
      // with the upgraded (identity-only) session. Record + emit so tests can
      // assert the interactive path fired.
      calls.requestScopes.push([scopes]);
      const s = makeSession({ scopes: ["openid", "profile", "email", "payments:write"] });
      client.emit("SIGNED_IN", s);
      return s;
    },
    async signOut(opts) {
      calls.signOut.push([opts]);
      client.emit("SIGNED_OUT", null);
    },
  };
  return client;
}

// ── probe: surface the context value for assertions ──────────────────────────

let observed: AuthContextValue | null = null;
function Probe(): React.ReactElement | null {
  observed = useAuth();
  return React.createElement("div", { "data-testid": "probe" }, observed.user?.id ?? "anon");
}

function setURL(url: string): void {
  // happy-dom: change the document URL (location.search) the mount path reads.
  (window as unknown as { happyDOM: { setURL(u: string): void } }).happyDOM.setURL(url);
}

beforeEach(() => {
  observed = null;
  setURL("https://myapp.zeroship.test/");
});

afterEach(() => {
  cleanup();
});

// ── tests ────────────────────────────────────────────────────────────────────

describe("AuthProvider mount + reactive state", () => {
  test("starts isLoading, then settles anonymous when checkSession finds nothing", async () => {
    const client = makeFakeClient();
    await act(async () => {
      render(
        React.createElement(
          AuthProvider,
          { client },
          React.createElement(Probe, null),
        ),
      );
    });
    assert.equal(client.calls.checkSession, 1, "mount must call checkSession on the no-params path");
    assert.equal(observed?.isLoading, false, "loading must clear after checkSession settles");
    assert.equal(observed?.isAuthenticated, false);
    assert.equal(observed?.user, null);
  });

  test("a failed recovery probe settles signed-out WITHOUT a user-facing error", async () => {
    // Regression: a transient mount-time checkSession failure (backend briefly
    // unreachable on first paint, or a session-mint network_error) must NOT
    // surface as `error` — that is what put "session mint request failed" on the
    // login page. Only an active sign-in completion (redirect/popup exchange)
    // should set `error`; a background recovery probe simply means "signed out".
    const client = makeFakeClient();
    client.checkSessionReject = new AuthError("network_error", "session mint request failed");
    await act(async () => {
      render(
        React.createElement(AuthProvider, { client }, React.createElement(Probe, null)),
      );
    });
    assert.equal(client.calls.checkSession, 1);
    assert.equal(observed?.isLoading, false, "loading clears even when the probe rejects");
    assert.equal(observed?.isAuthenticated, false);
    assert.equal(observed?.error, null, "a background recovery-probe failure is NOT a user-facing error");
  });

  test("transitions isLoading → authenticated when the client fires SIGNED_IN", async () => {
    const client = makeFakeClient();
    await act(async () => {
      render(
        React.createElement(AuthProvider, { client }, React.createElement(Probe, null)),
      );
    });
    assert.equal(observed?.isAuthenticated, false, "anonymous before any SIGNED_IN");

    // A real exchange / silent-renewal emits SIGNED_IN with the session.
    const session = makeSession();
    await act(async () => {
      client.emit("SIGNED_IN", session);
    });

    assert.equal(observed?.isAuthenticated, true);
    assert.equal(observed?.isLoading, false);
    assert.equal(observed?.user?.id, "pws_alice");
    assert.deepEqual(observed?.session?.scopes, ["openid", "profile", "email"]);
    assert.equal(
      "access_token" in (observed?.session as unknown as Record<string, unknown>),
      false,
      "BFF model: the React session snapshot carries no token",
    );
    assert.equal(screen.getByTestId("probe").textContent, "pws_alice");
  });

  test("checkSession that resolves a session lands authenticated on mount", async () => {
    const client = makeFakeClient();
    client.checkSessionResult = makeSession();
    await act(async () => {
      render(
        React.createElement(AuthProvider, { client }, React.createElement(Probe, null)),
      );
    });
    assert.equal(observed?.isAuthenticated, true);
    assert.equal(observed?.isLoading, false);
    assert.equal(observed?.user?.id, "pws_alice");
  });

  test("RECOVERING keeps isLoading true without an error", async () => {
    const client = makeFakeClient();
    await act(async () => {
      render(
        React.createElement(AuthProvider, { client }, React.createElement(Probe, null)),
      );
    });
    await act(async () => {
      client.emit("RECOVERING", null);
    });
    assert.equal(observed?.isLoading, true);
    assert.equal(observed?.error, null);
    assert.equal(observed?.isAuthenticated, false);
  });

  test("unsubscribes onAuthStateChange on unmount", async () => {
    const client = makeFakeClient();
    let unmount = () => {};
    await act(async () => {
      const r = render(
        React.createElement(AuthProvider, { client }, React.createElement(Probe, null)),
      );
      unmount = r.unmount;
    });
    assert.equal(client.listenerCount(), 1, "provider subscribes once while mounted");
    await act(async () => {
      unmount();
    });
    assert.equal(client.listenerCount(), 0, "provider must unsubscribe on unmount");
  });
});

describe("useAuth guard", () => {
  test("throws a clear error when used outside an AuthProvider", () => {
    assert.throws(
      () => {
        // Render the Probe with NO provider — useAuth must throw.
        act(() => {
          render(React.createElement(Probe, null));
        });
      },
      (e: unknown) => {
        assert.ok(e instanceof Error);
        assert.match((e as Error).message, /must be used within an <AuthProvider>/);
        return true;
      },
    );
  });
});

describe("SignInButton — gesture preservation", () => {
  test("click calls client.signInWithOAuth SYNCHRONOUSLY in the handler", async () => {
    const client = makeFakeClient();
    await act(async () => {
      render(
        React.createElement(
          AuthProvider,
          { client },
          React.createElement(
            SignInButton,
            { provider: "google", scopes: ["openid", "email"] },
            "Continue with Google",
          ),
        ),
      );
    });

    const btn = screen.getByRole("button", { name: "Continue with Google" });

    // Fire a real click and assert the client method was invoked SYNCHRONOUSLY
    // (before control returns from fireEvent / any await). This is the
    // popup-not-blocked guarantee: the call happens inside the gesture.
    assert.equal(client.calls.signInWithOAuth.length, 0, "not called before the click");
    fireEvent.click(btn);
    assert.equal(
      client.calls.signInWithOAuth.length,
      1,
      "signInWithOAuth must fire synchronously in the click handler",
    );
    assert.deepEqual(client.calls.signInWithOAuth[0][0], {
      provider: "google",
      scopes: ["openid", "email"],
      popup: undefined,
      prompt: undefined,
    });
    // Let the (resolved) sign-in promise settle so no unhandled rejection leaks.
    await act(async () => {
      await Promise.resolve();
    });
  });

  test("routes a sign-in rejection to onError", async () => {
    const client = makeFakeClient();
    client.signInError = new AuthError("popup_blocked", "blocked");
    let captured: AuthError | null = null;
    await act(async () => {
      render(
        React.createElement(
          AuthProvider,
          { client },
          React.createElement(
            SignInButton,
            { onError: (e) => (captured = e) },
            "Sign in",
          ),
        ),
      );
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Sign in" }));
      await Promise.resolve();
    });
    assert.ok(captured, "onError must receive the rejection");
    assert.equal((captured as unknown as AuthError).code, "popup_blocked");
  });
});

describe("SignOutButton", () => {
  test("click calls signOut and state flips to anonymous on SIGNED_OUT", async () => {
    const client = makeFakeClient();
    client.checkSessionResult = makeSession();
    await act(async () => {
      render(
        React.createElement(
          AuthProvider,
          { client },
          React.createElement(Probe, null),
          React.createElement(SignOutButton, { key: "so" }, "Log out"),
        ),
      );
    });
    assert.equal(observed?.isAuthenticated, true, "authenticated after checkSession");

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Log out" }));
      await Promise.resolve();
    });

    assert.equal(client.calls.signOut.length, 1, "signOut must be called");
    assert.equal(observed?.isAuthenticated, false, "state flips to anonymous on SIGNED_OUT");
    assert.equal(observed?.user, null);
  });
});

describe("requestScopes — interactive step-up (NO token to the browser)", () => {
  test("useAuth().requestScopes triggers an interactive step-up; the snapshot upgrades without a token", async () => {
    const client = makeFakeClient();
    await act(async () => {
      render(
        React.createElement(AuthProvider, { client }, React.createElement(Probe, null)),
      );
    });

    assert.equal(typeof observed?.requestScopes, "function", "exposed on the context value");
    // BFF invariant: the React context exposes NO token accessor.
    assert.equal(
      (observed as unknown as Record<string, unknown>).getAccessToken,
      undefined,
      "getAccessToken must not exist on the React context",
    );
    assert.equal(
      (observed as unknown as Record<string, unknown>).getAccessTokenWithPopup,
      undefined,
      "getAccessTokenWithPopup must not exist on the React context",
    );

    await act(async () => {
      await observed!.requestScopes(["payments:write"]);
    });

    // It must route through the INTERACTIVE step-up path (requestScopes).
    assert.equal(client.calls.requestScopes.length, 1, "must drive an interactive step-up");
    assert.deepEqual(client.calls.requestScopes[0], [["payments:write"]]);
    // The step-up emitted SIGNED_IN → the snapshot reflects the upgraded scopes,
    // and carries NO token.
    assert.ok(observed?.session?.scopes.includes("payments:write"));
    assert.equal(
      "access_token" in (observed?.session as unknown as Record<string, unknown>),
      false,
      "BFF model: no token on the upgraded session",
    );
  });
});

describe("<AuthModal> — hosts the cross-origin login iframe + Continue with Google", () => {
  test("on open it launches the immersive password flow (signInWithOAuth provider:password)", async () => {
    const client = makeFakeClient();
    client.oauthEmitsSignedIn = true;
    let succeeded = false;
    await act(async () => {
      render(
        React.createElement(
          AuthProvider,
          { client },
          React.createElement(AuthModal, { open: true, onSuccess: () => (succeeded = true) }),
        ),
      );
    });

    // The modal drove the first-party password sign-in (the iframe path) on open.
    const passwordCalls = client.calls.signInWithOAuth.filter(
      (c) => (c[0] as { provider?: string } | undefined)?.provider === "password",
    );
    assert.equal(passwordCalls.length, 1, "open launches signInWithOAuth({provider:'password'}) once");

    // Modal chrome: an accessible close affordance + the iframe host slot.
    assert.ok(screen.getByTestId("auth-modal-close"), "modal renders a close button");
    assert.equal(screen.getByTestId("auth-modal-close").getAttribute("aria-label"), "Close");
    assert.ok(screen.getByTestId("auth-iframe-host"), "modal renders the iframe host slot");
    // It is NOT the old credential form — no email/password inputs exist.
    assert.equal(screen.queryByLabelText("Email"), null, "no in-page email field (credential form deleted)");
    assert.equal(screen.queryByLabelText("Password"), null, "no in-page password field");

    // The fake emitted SIGNED_IN as a real iframe flow would → onSuccess fired.
    await waitFor(() => assert.equal(succeeded, true, "onSuccess fires once the iframe flow resolves"));
    assert.equal(observed === null, true, "no Probe rendered (modal-only tree)");
  });

  test("the close affordance invokes onClose (the caller dismisses the modal)", async () => {
    const client = makeFakeClient();
    let closed = false;
    await act(async () => {
      render(
        React.createElement(
          AuthProvider,
          { client },
          React.createElement(AuthModal, { open: true, onClose: () => (closed = true) }),
        ),
      );
    });
    fireEvent.click(screen.getByTestId("auth-modal-close"));
    assert.equal(closed, true, "close button fires onClose");
    await act(async () => {
      await Promise.resolve();
    });
  });

  test("Continue with Google launches the federated popup path", async () => {
    const client = makeFakeClient();
    await act(async () => {
      render(
        React.createElement(AuthProvider, { client }, React.createElement(AuthModal, { open: true })),
      );
    });

    const google = screen.getByRole("button", { name: "Continue with Google" });
    fireEvent.click(google);

    const googleCalls = client.calls.signInWithOAuth.filter(
      (c) => (c[0] as { provider?: string } | undefined)?.provider === "google",
    );
    assert.equal(googleCalls.length, 1, "Google routes through the federated popup OAuth path");
    assert.deepEqual(googleCalls[0][0], { provider: "google" });
    await act(async () => {
      await Promise.resolve();
    });
  });

  test("open=false renders nothing and does not launch a flow", async () => {
    const client = makeFakeClient();
    await act(async () => {
      render(
        React.createElement(AuthProvider, { client }, React.createElement(AuthModal, { open: false })),
      );
    });
    assert.equal(screen.queryByTestId("auth-iframe-host"), null, "closed modal renders no iframe host");
    assert.equal(
      screen.queryByRole("button", { name: "Continue with Google" }),
      null,
      "closed modal renders no Google button",
    );
    assert.equal(client.calls.signInWithOAuth.length, 0, "a closed modal launches no sign-in flow");
  });
});

describe("<AuthModal> — REAL client: the iframe mounts INTO the host slot (no overlay)", () => {
  // Faithful: build the REAL headless client via the `options` path so the
  // provider's auto-wiring (`iframeMount`/`iframeCancelled` → the modal slot)
  // runs, and let the DEFAULT iframe factory (env.ts) execute against happy-dom.
  // The iframe must land INSIDE the `auth-iframe-host` slot — NOT as a
  // full-viewport overlay on document.body that would cover the close button
  // (§8/§10.5). A stubbed global fetch keeps the exchange off the network; the
  // relay is driven by a real `message` event on the top window.
  const SAME_SITE_AUTH = "https://auth.zeroship.test";
  let realFetch: typeof globalThis.fetch;

  beforeEach(() => {
    setURL("https://console.zeroship.test/login");
    realFetch = globalThis.fetch;
  });
  afterEach(() => {
    globalThis.fetch = realFetch;
  });

  /** The OAuth `state` the SDK stashed for the in-flight transaction. */
  function pendingState(): string {
    for (let i = 0; i < window.sessionStorage.length; i++) {
      const key = window.sessionStorage.key(i);
      if (key && key.includes("zsauth") && key.includes("txn")) {
        try {
          const txn = JSON.parse(window.sessionStorage.getItem(key)!);
          if (txn && typeof txn.state === "string") return txn.state;
        } catch {
          /* not a txn record */
        }
      }
    }
    throw new Error("no pending PKCE transaction found");
  }

  test("the immersive iframe is appended inside auth-iframe-host, fills it, and is NOT a fixed overlay", async () => {
    // Stub the exchange so a relayed code resolves a session without a network.
    globalThis.fetch = (async () =>
      new Response(
        JSON.stringify({
          user: {
            id: "pws_alice",
            email: "alice@relay.zeroship.test",
            email_verified: true,
            name: "Alice",
            avatar: null,
            scopes: ["openid", "profile", "email"],
          },
          expires_at: Math.floor(Date.now() / 1000) + 600,
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      )) as unknown as typeof globalThis.fetch;

    await act(async () => {
      render(
        React.createElement(
          AuthProvider,
          {
            options: {
              appOrigin: "https://console.zeroship.test",
              authOrigin: SAME_SITE_AUTH,
              immersive: true,
            },
          },
          React.createElement(AuthModal, { open: true, hideOAuth: true }),
        ),
      );
    });

    const host = screen.getByTestId("auth-iframe-host");

    // The default factory mounts the iframe INTO the slot (after beginFlow's
    // async PKCE), not onto document.body as a fixed overlay.
    let iframe: HTMLIFrameElement | null = null;
    await waitFor(() => {
      iframe = host.querySelector("iframe");
      assert.ok(iframe, "the immersive iframe must mount inside the auth-iframe-host slot");
    });
    // It is NOT a full-viewport fixed overlay (that would paint over the close
    // button) — the in-slot variant carries no position:fixed / max z-index.
    const style = iframe!.getAttribute("style") ?? "";
    assert.ok(!/position\s*:\s*fixed/i.test(style), `in-slot iframe must not be position:fixed; got ${style}`);
    assert.ok(!/z-index\s*:\s*2147483647/.test(style), "in-slot iframe must not use the max z-index overlay");
    // The src is the cross-origin authorize URL on the app origin (element src,
    // never contentWindow.location) — the §4.1 navigation contract.
    assert.match(
      iframe!.getAttribute("src") ?? "",
      /\/__zeroship\/auth\/authorize\?/,
      "the iframe src is the app-origin authorize URL",
    );
    // No iframe leaked onto document.body as a sibling overlay.
    assert.equal(
      document.body.querySelector(":scope > iframe"),
      null,
      "no full-viewport overlay iframe is appended directly to <body>",
    );

    // Settle the flow with a real relay message so no timer leaks, then assert
    // SIGNED_IN propagated through the provider (state sync intact).
    const state = pendingState();
    await act(async () => {
      window.dispatchEvent(
        new MessageEvent("message", {
          origin: "https://console.zeroship.test",
          data: { type: "zs:authorization_response", response: { code: "real-code", state } },
        }),
      );
      await new Promise((r) => setTimeout(r, 0));
    });
    await waitFor(() => {
      assert.equal(host.querySelector("iframe"), null, "the iframe is torn down once the relay settles");
    });
  });

  test("dismissing the modal (close button) cancels the in-flight flow and tears the iframe down", async () => {
    // The exchange must never be reached — the user cancels first.
    globalThis.fetch = (async () => {
      throw new Error("exchange must not run on cancel");
    }) as unknown as typeof globalThis.fetch;

    let closed = false;
    await act(async () => {
      render(
        React.createElement(
          AuthProvider,
          {
            options: {
              appOrigin: "https://console.zeroship.test",
              authOrigin: SAME_SITE_AUTH,
              immersive: true,
            },
          },
          React.createElement(AuthModal, {
            open: true,
            hideOAuth: true,
            onClose: () => (closed = true),
          }),
        ),
      );
    });

    const host = screen.getByTestId("auth-iframe-host");
    await waitFor(() => {
      assert.ok(host.querySelector("iframe"), "iframe mounted before cancel");
    });

    // Click close → onClose fires AND the cancel signal resolves → runIframe
    // rejects popup_closed → the iframe is removed (§8).
    await act(async () => {
      fireEvent.click(screen.getByTestId("auth-modal-close"));
      await new Promise((r) => setTimeout(r, 0));
    });
    assert.equal(closed, true, "the close button invokes onClose");
    await waitFor(() => {
      assert.equal(host.querySelector("iframe"), null, "the iframe is torn down on cancel");
    });
  });
});

describe("SignIn convenience component", () => {
  test("renders a default sign-in launcher that opens the popup flow on click", async () => {
    const client = makeFakeClient();
    await act(async () => {
      render(
        React.createElement(AuthProvider, { client }, React.createElement(SignIn, { scopes: ["openid", "email"] })),
      );
    });

    const btn = screen.getByRole("button", { name: "Sign in" });
    fireEvent.click(btn);

    assert.equal(client.calls.signInWithOAuth.length, 1, "SignIn click drives signInWithOAuth synchronously");
    assert.deepEqual(client.calls.signInWithOAuth[0][0], {
      provider: undefined,
      scopes: ["openid", "email"],
      popup: undefined,
      prompt: undefined,
    });
    await act(async () => {
      await Promise.resolve();
    });
  });
});

describe("SignedIn / SignedOut conditional rendering", () => {
  test("SignedOut shows when anonymous; SignedIn shows after SIGNED_IN", async () => {
    const client = makeFakeClient();
    await act(async () => {
      render(
        React.createElement(
          AuthProvider,
          { client },
          React.createElement(SignedIn, { key: "in" }, React.createElement("span", { "data-testid": "in" }, "in")),
          React.createElement(SignedOut, { key: "out" }, React.createElement("span", { "data-testid": "out" }, "out")),
        ),
      );
    });
    assert.equal(screen.queryByTestId("in"), null, "SignedIn hidden while anonymous");
    assert.ok(screen.queryByTestId("out"), "SignedOut visible while anonymous");

    await act(async () => {
      client.emit("SIGNED_IN", makeSession());
    });
    assert.ok(screen.queryByTestId("in"), "SignedIn visible after SIGNED_IN");
    assert.equal(screen.queryByTestId("out"), null, "SignedOut hidden after SIGNED_IN");
  });
});

describe("redirect-callback mount path (?code=&state=)", () => {
  test("calls exchangeCodeForSession then strips the query via replaceState", async () => {
    const client = makeFakeClient();
    setURL("https://myapp.zeroship.test/dashboard?code=AUTH_CODE&state=ST8&iss=hydra");

    await act(async () => {
      render(
        React.createElement(AuthProvider, { client }, React.createElement(Probe, null)),
      );
    });

    assert.equal(
      client.calls.exchangeCodeForSession.length,
      1,
      "the code+state URL must drive the redirect-callback exchange",
    );
    assert.deepEqual(client.calls.exchangeCodeForSession[0], ["AUTH_CODE", "ST8"]);
    assert.equal(client.calls.checkSession, 0, "must NOT also call checkSession on the redirect path");

    // The query was stripped without a navigation; the path/hash survive.
    assert.equal(window.location.pathname, "/dashboard");
    assert.equal(window.location.search, "", "code/state/iss must be stripped from the URL");

    // The exchange emitted SIGNED_IN → authenticated.
    assert.equal(observed?.isAuthenticated, true);
    assert.equal(observed?.user?.id, "pws_alice");
  });

  test("an exchange failure still strips the query and surfaces the error", async () => {
    const client = makeFakeClient();
    client.exchangeError = new AuthError("invalid_grant", "code spent");
    setURL("https://myapp.zeroship.test/cb?code=BAD&state=ST9");

    await act(async () => {
      render(
        React.createElement(AuthProvider, { client }, React.createElement(Probe, null)),
      );
    });

    assert.equal(client.calls.exchangeCodeForSession.length, 1);
    assert.equal(window.location.search, "", "query stripped even on failure (no replay)");
    assert.equal(observed?.isLoading, false);
    assert.equal(observed?.error?.code, "invalid_grant");
    assert.equal(observed?.isAuthenticated, false);
  });
});

describe("provider drives the REAL headless client end-to-end", () => {
  test("a real createAuthClient + checkSession 401 settles anonymous (no error)", async () => {
    // Faithful: build the REAL headless client over the test harness env, then
    // hand it to the provider. The mount path runs the real `checkSession` →
    // `/__zeroship/auth/session?mint=1`; a 401 maps to `login_required` → clean
    // anonymous. This exercises the actual client+provider integration, not a
    // fake. (The `options` path constructs the same client; we inject the env
    // here only to keep the probe off the network and deterministic.)
    const h = makeHarness();
    h.fetch.on("/__zeroship/auth/session", () =>
      jsonResponse(401, { error: "login_required", error_description: "no session" }),
    );
    const realClient = createAuthClient({ appOrigin: "https://myapp.zeroship.ai" }, h.env);

    await act(async () => {
      render(
        React.createElement(AuthProvider, { client: realClient }, React.createElement(Probe, null)),
      );
    });

    // The mount effect's async probe (real checkSession → /session 401) settles
    // in its own microtask batch. Rely on RTL's `waitFor` (which polls inside
    // `act`) rather than a raw `setTimeout(0)` inside `act` — the latter is
    // timing-dependent under React 18/19's concurrent scheduler.
    await waitFor(() => {
      assert.equal(observed?.isLoading, false, "loading clears once the 401 probe settles");
    });

    assert.ok(observed, "useAuth resolved a context value from the real client");
    assert.equal(observed?.isAuthenticated, false);
    assert.equal(observed?.error, null, "login_required is a clean anonymous, not an error");
    assert.equal(typeof observed?.signInWithOAuth, "function");
    assert.ok(
      h.fetch.requests.some((r) => r.url.includes("/__zeroship/auth/session")),
      "the real client actually probed /session on mount",
    );
  });
});
