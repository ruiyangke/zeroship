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
    access_token: "at_abc",
    expires_at: Math.floor(Date.now() / 1000) + 3600,
    token_type: "Bearer",
    user: USER,
    scopes: ["openid", "profile", "email"],
    ...over,
  };
}

// ── fake AuthClient (real emitter) ───────────────────────────────────────────

type Listener = (event: AuthChangeEvent, session: Session | null) => void;

interface CallLog {
  signInWithOAuth: unknown[][];
  signInWithPassword: unknown[][];
  signOut: unknown[][];
  checkSession: number;
  exchangeCodeForSession: Array<[string, string | undefined]>;
  requestScopes: unknown[][];
  getAccessTokenWithPopup: unknown[][];
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
}

function makeFakeClient(): FakeClient {
  const listeners = new Set<Listener>();
  const calls: CallLog = {
    signInWithOAuth: [],
    signInWithPassword: [],
    signOut: [],
    checkSession: 0,
    exchangeCodeForSession: [],
    requestScopes: [],
    getAccessTokenWithPopup: [],
  };
  let current: Session | null = null;

  const client: FakeClient = {
    calls,
    checkSessionResult: null,
    signInError: null,
    exchangeError: null,

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
      return makeSession();
    },
    async signInWithPassword(opts) {
      calls.signInWithPassword.push([opts]);
      return makeSession();
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
      client.emit("TOKEN_REFRESHED", s);
      return s;
    },
    async getAccessToken() {
      return current?.access_token ?? "at_abc";
    },
    async getAccessTokenWithPopup(opts) {
      // Auth0 parity: an interactive step-up that routes through requestScopes.
      calls.getAccessTokenWithPopup.push([opts]);
      const s = await client.requestScopes(opts?.scopes ?? ["openid", "profile", "email"]);
      return s.access_token;
    },
    isAuthenticated() {
      return current != null;
    },
    hasScope(scope) {
      return current?.scopes.includes(scope) ?? false;
    },
    async requestScopes(scopes) {
      // Interactive step-up: a real client opens a popup and emits SIGNED_IN
      // with the upgraded session. Record + emit so tests can assert the
      // interactive path fired.
      calls.requestScopes.push([scopes]);
      const s = makeSession({ access_token: "at_stepup" });
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
    assert.equal(observed?.session?.access_token, "at_abc");
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

describe("getAccessTokenWithPopup — interactive step-up", () => {
  test("useAuth().getAccessTokenWithPopup triggers an interactive step-up and returns the token", async () => {
    const client = makeFakeClient();
    await act(async () => {
      render(
        React.createElement(AuthProvider, { client }, React.createElement(Probe, null)),
      );
    });

    assert.equal(typeof observed?.getAccessTokenWithPopup, "function", "exposed on the context value");

    let token: string | undefined;
    await act(async () => {
      token = await observed!.getAccessTokenWithPopup({ scopes: ["payments:write"] });
    });

    // It must route through the INTERACTIVE step-up path (requestScopes), not a
    // silent mint — that is the whole point of the *WithPopup variant.
    assert.equal(client.calls.getAccessTokenWithPopup.length, 1);
    assert.equal(client.calls.requestScopes.length, 1, "must drive an interactive step-up");
    assert.deepEqual(client.calls.requestScopes[0], [["payments:write"]]);
    assert.equal(token, "at_stepup", "returns the freshly-minted access token");
    // The step-up emitted SIGNED_IN → the snapshot reflects the upgraded session.
    assert.equal(observed?.session?.access_token, "at_stepup");
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
    // `/__zs/auth/session?mint=1`; a 401 maps to `login_required` → clean
    // anonymous. This exercises the actual client+provider integration, not a
    // fake. (The `options` path constructs the same client; we inject the env
    // here only to keep the probe off the network and deterministic.)
    const h = makeHarness();
    h.fetch.on("/__zs/auth/session", () =>
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
      h.fetch.requests.some((r) => r.url.includes("/__zs/auth/session")),
      "the real client actually probed /session on mount",
    );
  });
});
