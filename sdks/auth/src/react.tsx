/**
 * `@zeroship/auth/react` — the React adapter (`./react` subpath).
 *
 * A thin, reactive layer over the headless {@link AuthClient} (`./client`).
 * The shape mirrors `@auth0/auth0-react`: an {@link AuthProvider} builds (or
 * receives) one client, drives mount-time recovery, and publishes a reactive
 * `{ user, session, isAuthenticated, isLoading, error }` snapshot through
 * context; {@link useAuth} reads that snapshot plus the client methods bound to
 * it; {@link SignInButton}/{@link SignOutButton} invoke `signInWithOAuth` /
 * `signOut` INSIDE the click handler so the popup opens in the user gesture and
 * the browser does not block it.
 *
 * This entry is pure browser code — it has NO runtime dependency on `zeroship`
 * (the server entry). React is an optional peer (`peerDependenciesMeta`).
 */

import {
  createContext,
  createElement,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ButtonHTMLAttributes,
  type ReactNode,
} from "react";

import { createAuthClient, type AuthClient } from "./client";
import {
  AuthError,
  type AuthChangeEvent,
  type AuthClientOptions,
  type Session,
  type SignInOptions,
  type SignOutOptions,
  type User,
} from "./types";

// ── context value ──────────────────────────────────────────────────────────

/**
 * The value published by {@link AuthProvider} and read by {@link useAuth}: the
 * reactive auth snapshot plus the client methods bound to the live client.
 */
export interface AuthContextValue {
  /** True when a non-expired session is held. */
  isAuthenticated: boolean;
  /** True until the mount-time recovery (`checkSession` / redirect exchange) settles. */
  isLoading: boolean;
  /** The last error from a mount-time recovery or a bound method, else null. */
  error: AuthError | null;
  /** The server-validated user, or null when anonymous. */
  user: User | null;
  /** The live session, or null when anonymous. */
  session: Session | null;

  /** Interactive popup sign-in (gesture-safe when called from a click handler). */
  signInWithOAuth(opts?: SignInOptions): Promise<void>;
  /** Phase-1 hosted-password sign-in (popup flow with `provider=password`). */
  signInWithPassword(opts?: { scopes?: string[]; popup?: boolean }): Promise<void>;
  /** Local (default) or global sign-out. */
  signOut(opts?: SignOutOptions): Promise<void>;
  /** A valid access token, refreshing under the lock if near expiry. */
  getAccessToken(): Promise<string>;
  /** Step-up: acquire a token with additional scopes via interactive popup (Auth0 parity). */
  getAccessTokenWithPopup(opts?: { scopes?: string[] }): Promise<string>;
  /** Does the current session carry this scope? (synchronous, cache-derived) */
  hasScope(scope: string): boolean;
}

const AuthContext = createContext<AuthContextValue | null>(null);

// ── reactive snapshot ───────────────────────────────────────────────────────

interface AuthState {
  isAuthenticated: boolean;
  isLoading: boolean;
  error: AuthError | null;
  user: User | null;
  session: Session | null;
}

const INITIAL: AuthState = {
  isAuthenticated: false,
  isLoading: true,
  error: null,
  user: null,
  session: null,
};

/** Project a Session (or null) into the reactive snapshot, preserving `isLoading`/`error`. */
function fromSession(session: Session | null, isLoading: boolean): AuthState {
  return {
    isAuthenticated: session != null,
    isLoading,
    error: null,
    user: session?.user ?? null,
    session,
  };
}

/**
 * True when the current page URL is a full-page-redirect return — it carries
 * BOTH an authorization `code` and a `state` (or an OAuth `error`) in the query
 * string. Mirrors `@auth0/auth0-react`'s `hasAuthParams`.
 */
export function hasAuthParams(search: string = window.location.search): boolean {
  const params = new URLSearchParams(search);
  return (
    (params.has("code") && params.has("state")) ||
    (params.has("error") && params.has("state"))
  );
}

/** Strip the OAuth query (`code`/`state`/`error`/…) without a navigation. */
function stripAuthParams(): void {
  const url = new URL(window.location.href);
  for (const key of ["code", "state", "error", "error_description", "iss"]) {
    url.searchParams.delete(key);
  }
  const next = url.pathname + (url.search ? url.search : "") + url.hash;
  window.history.replaceState(window.history.state, "", next);
}

function toAuthError(e: unknown): AuthError {
  return e instanceof AuthError
    ? e
    : new AuthError("server_error", e instanceof Error ? e.message : String(e), { cause: e });
}

// ── provider ─────────────────────────────────────────────────────────────────

export interface AuthProviderProps {
  children?: ReactNode;
  /** Options for the client this provider builds (once). Ignored if `client` is supplied. */
  options?: AuthClientOptions;
  /**
   * An injected, pre-built client — for tests/SSR only. When present the provider
   * does NOT build its own and `options` is ignored.
   *
   * @internal Not part of the public surface (§4.2 lists only `children`/`options`);
   *   exposed for test injection and hidden from downstream API tooling.
   */
  client?: AuthClient;
}

/**
 * Builds (or receives) a single {@link AuthClient}, runs mount-time recovery,
 * subscribes to `onAuthStateChange`, and publishes the reactive snapshot.
 *
 * Mount lifecycle (auth0-react parity):
 *  1. If the URL has `?code=&state=` (or `?error=&state=`) — a full-page
 *     redirect return — call `client.exchangeCodeForSession(code, state)` then
 *     `history.replaceState` to strip the query.
 *  2. Otherwise call `client.checkSession()` (breadcrumb-gated rehydration).
 *  3. Either way, subscribe to `onAuthStateChange` and mirror every event into
 *     React state; unsubscribe on unmount.
 */
export function AuthProvider(props: AuthProviderProps): ReactNode {
  // Build the client EXACTLY ONCE. An injected `client` wins (tests/SSR);
  // otherwise `createAuthClient(options)` runs in the lazy initializer so it is
  // not re-created on every render.
  const [client] = useState<AuthClient>(
    () => props.client ?? createAuthClient(props.options),
  );

  const [state, setState] = useState<AuthState>(INITIAL);
  // Guard against a state update after unmount (StrictMode double-invoke / async settle).
  const mountedRef = useRef(true);

  useEffect(() => {
    mountedRef.current = true;

    // Subscribe FIRST so a SIGNED_IN/TOKEN_REFRESHED emitted by the recovery
    // path below (exchange / checkSession both emit) is never missed.
    const sub = client.onAuthStateChange((event: AuthChangeEvent, session) => {
      if (!mountedRef.current) return;
      setState((prev) => {
        if (event === "RECOVERING") {
          // Still settling a 503-provisioning probe — keep loading, no error.
          return { ...prev, isLoading: true, error: null };
        }
        // SIGNED_IN | SIGNED_OUT | TOKEN_REFRESHED | USER_UPDATED — the session
        // argument is authoritative; recovery has settled.
        return fromSession(session, false);
      });
    });

    void (async () => {
      try {
        if (hasAuthParams()) {
          const params = new URLSearchParams(window.location.search);
          const code = params.get("code");
          const state = params.get("state") ?? undefined;
          // Strip the query regardless of outcome so a reload can't re-run the
          // exchange against a spent code.
          if (code) {
            try {
              await client.exchangeCodeForSession(code, state);
            } finally {
              stripAuthParams();
            }
          } else {
            // `?error=&state=` — surface the OAuth error, then strip.
            stripAuthParams();
            const errCode = params.get("error") ?? "server_error";
            throw new AuthError(
              "server_error",
              params.get("error_description") ?? errCode,
            );
          }
        } else {
          await client.checkSession();
        }
        // `onAuthStateChange` has already pushed the authenticated snapshot via
        // the subscription above; just clear the loading flag if no event did.
        if (mountedRef.current) {
          setState((prev) => (prev.isLoading ? { ...prev, isLoading: false } : prev));
        }
      } catch (e) {
        if (mountedRef.current) {
          setState((prev) => ({ ...prev, isLoading: false, error: toAuthError(e) }));
        }
      }
    })();

    return () => {
      mountedRef.current = false;
      sub.unsubscribe();
    };
  }, [client]);

  // Bound methods. `void`-returning wrappers (signInWithOAuth / signInWithPassword
  // / signOut) surface their own errors into `state.error` so a fire-and-forget
  // onClick caller doesn't leave an unhandled rejection.
  const signInWithOAuth = useCallback(
    async (opts?: SignInOptions): Promise<void> => {
      try {
        await client.signInWithOAuth(opts);
      } catch (e) {
        if (mountedRef.current) setState((prev) => ({ ...prev, error: toAuthError(e) }));
        throw toAuthError(e);
      }
    },
    [client],
  );

  const signInWithPassword = useCallback(
    async (opts?: { scopes?: string[]; popup?: boolean }): Promise<void> => {
      try {
        await client.signInWithPassword(opts);
      } catch (e) {
        if (mountedRef.current) setState((prev) => ({ ...prev, error: toAuthError(e) }));
        throw toAuthError(e);
      }
    },
    [client],
  );

  const signOut = useCallback(
    async (opts?: SignOutOptions): Promise<void> => {
      try {
        await client.signOut(opts);
      } catch (e) {
        if (mountedRef.current) setState((prev) => ({ ...prev, error: toAuthError(e) }));
        throw toAuthError(e);
      }
    },
    [client],
  );

  const getAccessToken = useCallback(() => client.getAccessToken(), [client]);
  const getAccessTokenWithPopup = useCallback(
    (opts?: { scopes?: string[] }) => client.getAccessTokenWithPopup(opts),
    [client],
  );
  // `hasScope` reads the client's live cache directly — it is intentionally NOT
  // derived from `state.session` so a step-up that mutated the cache before the
  // next render is reflected immediately.
  const hasScope = useCallback((scope: string) => client.hasScope(scope), [client]);

  // Memoize so the context value reference only changes when the auth snapshot
  // or a bound-method identity actually changes — without this, every parent
  // re-render would cascade to every `useAuth()` consumer (auth0-react/Supabase
  // both wrap their context value in useMemo for exactly this reason).
  const value = useMemo<AuthContextValue>(
    () => ({
      isAuthenticated: state.isAuthenticated,
      isLoading: state.isLoading,
      error: state.error,
      user: state.user,
      session: state.session,
      signInWithOAuth,
      signInWithPassword,
      signOut,
      getAccessToken,
      getAccessTokenWithPopup,
      hasScope,
    }),
    [
      state,
      signInWithOAuth,
      signInWithPassword,
      signOut,
      getAccessToken,
      getAccessTokenWithPopup,
      hasScope,
    ],
  );

  return createElement(AuthContext.Provider, { value }, props.children);
}

// ── hook ─────────────────────────────────────────────────────────────────────

/**
 * Read the reactive auth snapshot + bound client methods. Throws a clear error
 * if called outside an {@link AuthProvider}.
 */
export function useAuth(): AuthContextValue {
  const ctx = useContext(AuthContext);
  if (ctx === null) {
    throw new Error(
      "useAuth must be used within an <AuthProvider>. Wrap your app (or the " +
        "subtree that calls useAuth) in <AuthProvider>.",
    );
  }
  return ctx;
}

// ── components ─────────────────────────────────────────────────────────────────

// Drop the native `onClick` (we own it) and `onError` (we re-type it to receive
// an AuthError, not a DOM error event).
type ButtonProps = Omit<ButtonHTMLAttributes<HTMLButtonElement>, "onClick" | "onError">;

export interface SignInButtonProps extends ButtonProps {
  /** OAuth provider hint (`google` | `github` | `password`). */
  provider?: SignInOptions["provider"];
  /** Override the default scope set for this sign-in. */
  scopes?: string[];
  /** Default `true`. Popup vs full-page redirect. */
  popup?: boolean;
  /** OIDC `prompt` passthrough (step-up). */
  prompt?: SignInOptions["prompt"];
  /** Called if `signInWithOAuth` rejects (e.g. `popup_blocked`/`popup_closed`). */
  onError?: (error: AuthError) => void;
  children?: ReactNode;
}

/**
 * A button whose click handler calls `signInWithOAuth` SYNCHRONOUSLY (inside
 * the user gesture) so the popup opens before any await — the browser cannot
 * block it. Errors are routed to the optional `onError` callback.
 */
export function SignInButton(props: SignInButtonProps): ReactNode {
  const { provider, scopes, popup, prompt, onError, children, ...buttonProps } = props;
  const { signInWithOAuth } = useAuth();

  const onClick = useCallback(() => {
    // Invoke INSIDE the gesture; the client opens the popup synchronously before
    // its first await. Do NOT await here — that would defer to a microtask and
    // the popup would already be open; we only need to catch the rejection.
    signInWithOAuth({ provider, scopes, popup, prompt }).catch((e: unknown) => {
      onError?.(toAuthError(e));
    });
  }, [signInWithOAuth, provider, scopes, popup, prompt, onError]);

  return createElement(
    "button",
    { type: "button", ...buttonProps, onClick },
    children ?? "Sign in",
  );
}

export interface SignOutButtonProps extends ButtonProps {
  /** `local` (default) or `global` sign-out. */
  scope?: SignOutOptions["scope"];
  /** Called if `signOut` rejects. */
  onError?: (error: AuthError) => void;
  children?: ReactNode;
}

/** A button whose click handler calls `signOut`. Errors route to `onError`. */
export function SignOutButton(props: SignOutButtonProps): ReactNode {
  const { scope, onError, children, ...buttonProps } = props;
  const { signOut } = useAuth();

  const onClick = useCallback(() => {
    signOut({ scope }).catch((e: unknown) => {
      onError?.(toAuthError(e));
    });
  }, [signOut, scope, onError]);

  return createElement(
    "button",
    { type: "button", ...buttonProps, onClick },
    children ?? "Sign out",
  );
}

export interface SignInProps {
  /** Override the default scope set for this sign-in. */
  scopes?: string[];
}

/**
 * A zero-config sign-in launcher (spec §4.2 convenience component). Renders a
 * default {@link SignInButton} that opens the popup OAuth flow on click; pass
 * `scopes` to request a non-default scope set. For full control over the
 * provider/label/error handling, use {@link SignInButton} directly.
 */
export function SignIn(props: SignInProps): ReactNode {
  return createElement(SignInButton, { scopes: props.scopes });
}

/** Renders `children` only when authenticated. Headless (no markup of its own). */
export function SignedIn(props: { children?: ReactNode }): ReactNode {
  return useAuth().isAuthenticated ? props.children : null;
}

/** Renders `children` only when anonymous AND recovery has settled. Headless. */
export function SignedOut(props: { children?: ReactNode }): ReactNode {
  const { isAuthenticated, isLoading } = useAuth();
  return !isAuthenticated && !isLoading ? props.children : null;
}

// ── re-exports ─────────────────────────────────────────────────────────────────

export { createAuthClient, type AuthClient } from "./client";
export {
  AuthError,
  type AuthChangeEvent,
  type AuthClientOptions,
  type AuthErrorCode,
  type Session,
  type SignInOptions,
  type SignOutOptions,
  type User,
} from "./types";
