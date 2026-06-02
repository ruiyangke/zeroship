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

  /**
   * Interactive sign-in (gesture-safe when called from a click handler).
   * `provider:'password'` drives the first-party login UI (immersive iframe on
   * the same-site console, popup elsewhere); `'google'|'github'` are federated
   * popups.
   */
  signInWithOAuth(opts?: SignInOptions): Promise<void>;
  /** Local (default) or global sign-out. */
  signOut(opts?: SignOutOptions): Promise<void>;
  /** Step-up: re-consent additional scopes via interactive popup (server-side grant; no token to the browser). */
  requestScopes(scopes: string[]): Promise<void>;
  /** Does the current session carry this scope? (synchronous, cache-derived) */
  hasScope(scope: string): boolean;
}

const AuthContext = createContext<AuthContextValue | null>(null);

// ── immersive-iframe mount controller (internal) ─────────────────────────────

/**
 * The live mount target + cancel signal for the immersive login iframe, shared
 * between {@link AuthProvider} (which feeds it to the client's `iframeMount` /
 * `iframeCancelled` env handles) and {@link AuthModal} (which sets it on open).
 * Held in a ref so the client's option callbacks — closed over at client
 * construction — always read the CURRENT open modal's slot, with no client
 * rebuild (§4.1, §8).
 */
interface ModalMount {
  /** The host slot the iframe mounts INTO; null ⇒ no modal open (overlay fallback). */
  host: Element | null;
  /** Resolves when the user dismisses the open modal → `runIframe` rejects `popup_closed`. */
  cancelled: Promise<void> | undefined;
}

/** Internal: lets {@link AuthModal} register its slot with {@link AuthProvider}'s client. */
const ModalMountContext = createContext<{ current: ModalMount } | null>(null);

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
  // The live immersive-iframe mount target + cancel signal. The client's
  // `iframeMount`/`iframeCancelled` env handles read this ref at flow time, so
  // an `AuthModal` opened LATER can still steer the iframe into its slot without
  // rebuilding the client (§4.1, §8). Empty until a modal registers.
  const modalMountRef = useRef<ModalMount>({ host: null, cancelled: undefined });

  // Build the client EXACTLY ONCE. An injected `client` wins (tests/SSR);
  // otherwise `createAuthClient(options)` runs in the lazy initializer so it is
  // not re-created on every render. We merge the modal-mount handles into the
  // options UNLESS the consumer already supplied their own (explicit wins).
  const [client] = useState<AuthClient>(() => {
    if (props.client) return props.client;
    const options: AuthClientOptions = { ...props.options };
    if (!options.iframeMount) {
      options.iframeMount = () => modalMountRef.current.host;
    }
    if (!options.iframeCancelled) {
      options.iframeCancelled = () => modalMountRef.current.cancelled;
    }
    return createAuthClient(options);
  });

  const [state, setState] = useState<AuthState>(INITIAL);
  // Guard against a state update after unmount (StrictMode double-invoke / async settle).
  const mountedRef = useRef(true);

  useEffect(() => {
    mountedRef.current = true;

    // Subscribe FIRST so a SIGNED_IN/SESSION_REFRESHED emitted by the recovery
    // path below (exchange / checkSession both emit) is never missed.
    const sub = client.onAuthStateChange((event: AuthChangeEvent, session) => {
      if (!mountedRef.current) return;
      setState((prev) => {
        if (event === "RECOVERING") {
          // Still settling a 503-provisioning probe — keep loading, no error.
          return { ...prev, isLoading: true, error: null };
        }
        // SIGNED_IN | SIGNED_OUT | SESSION_REFRESHED | USER_UPDATED — the session
        // argument is authoritative; recovery has settled.
        return fromSession(session, false);
      });
    });

    // An active sign-in COMPLETION (redirect/popup exchange landing back with
    // `?code=&state=` or `?error=`) — its failure is a real, user-facing sign-in
    // error. A plain recovery probe (`checkSession`) is NOT: it just asks "is
    // there a session to restore?", and a `login_required` / briefly-unreachable
    // backend on first paint simply means "signed out" — never a scary banner on
    // a login page.
    const isSignInCompletion = hasAuthParams();
    void (async () => {
      try {
        if (isSignInCompletion) {
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
        if (!mountedRef.current) return;
        if (isSignInCompletion) {
          // The redirect/popup exchange failed — surface it so the user sees why.
          setState((prev) => ({ ...prev, isLoading: false, error: toAuthError(e) }));
        } else {
          // A background recovery probe failed (no session, or the backend was
          // briefly unreachable on first paint). Settle to a clean signed-out
          // state — no user-facing error — so the login UI renders normally.
          setState((prev) => ({ ...prev, isLoading: false, error: null }));
        }
      }
    })();

    return () => {
      mountedRef.current = false;
      sub.unsubscribe();
    };
  }, [client]);

  // Bound methods. `void`-returning wrappers (signInWithOAuth / signOut) surface
  // their own errors into `state.error` so a fire-and-forget onClick caller
  // doesn't leave an unhandled rejection.
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

  const requestScopes = useCallback(
    async (scopes: string[]): Promise<void> => {
      try {
        await client.requestScopes(scopes);
      } catch (e) {
        if (mountedRef.current) setState((prev) => ({ ...prev, error: toAuthError(e) }));
        throw toAuthError(e);
      }
    },
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
      signOut,
      requestScopes,
      hasScope,
    }),
    [state, signInWithOAuth, signOut, requestScopes, hasScope],
  );

  return createElement(
    AuthContext.Provider,
    { value },
    createElement(ModalMountContext.Provider, { value: modalMountRef }, props.children),
  );
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

// ── immersive login modal (hosts the cross-origin auth iframe) ───────────────

// The SDK can't depend on @zeroship/ui (it's framework-agnostic), so the modal
// ships unstyled-but-structured: stable class hooks (`zs-auth-*`) + a tiny
// scoped stylesheet a consumer fully overrides by re-declaring those classes.
// The modal hosts the cross-origin `auth.zeroship.ai/login` iframe; the
// password is typed INTO that auth-origin frame, so console JS never sees the
// credential (SOP, §6.1). The builder themes these with its crystal tokens.

/** Minimal, override-friendly default styling for the login modal chrome. */
const MODAL_STYLE = `
.zs-auth-modal-backdrop{position:fixed;inset:0;display:flex;align-items:center;justify-content:center;background:rgba(0,0,0,.45)}
.zs-auth-modal{background:#fff;color:#111;padding:1.5rem;border-radius:.75rem;max-width:26rem;width:100%;display:flex;flex-direction:column;gap:1rem;position:relative}
.zs-auth-modal-header{display:flex;align-items:center;justify-content:space-between;gap:1rem}
.zs-auth-title{font-size:1.125rem;font-weight:600;margin:0}
.zs-auth-close{background:none;border:0;font:inherit;font-size:1.25rem;line-height:1;cursor:pointer;padding:.25rem;border-radius:.375rem}
.zs-auth-divider{display:flex;align-items:center;gap:.5rem;font-size:.75rem;opacity:.7}
.zs-auth-divider::before,.zs-auth-divider::after{content:"";flex:1;height:1px;background:currentColor;opacity:.3}
.zs-auth-oauth{padding:.5rem .75rem;border-radius:.375rem;cursor:pointer;font:inherit;width:100%}
.zs-auth-frame{width:100%;min-height:24rem;display:block}
.zs-auth-frame iframe{display:block;width:100%;height:100%;min-height:24rem;border:0}
`;

/** Inject the default modal stylesheet once per document (no-op when present / no DOM). */
function useModalStyles(): void {
  useEffect(() => {
    if (typeof document === "undefined") return;
    const ID = "zs-auth-modal-styles";
    if (document.getElementById(ID)) return;
    const el = document.createElement("style");
    el.id = ID;
    el.textContent = MODAL_STYLE;
    document.head.appendChild(el);
  }, []);
}

export interface AuthModalProps {
  /** Render the overlay only when true. Default `true` (caller can keep it mounted-but-hidden). */
  open?: boolean;
  /**
   * Called when the user dismisses the modal (the close button or the backdrop).
   * The caller closes the modal; the in-flight iframe sign-in is cancelled
   * (rejects `popup_closed`) and the iframe torn down (§8).
   */
  onClose?: () => void;
  /** Called after a successful sign-in (SIGNED_IN already emitted). */
  onSuccess?: () => void;
  /** Heading rendered above the iframe. Default `"Sign in"`. */
  title?: ReactNode;
  /** Hide the "Continue with Google" federated button. Default `false`. */
  hideOAuth?: boolean;
  /** Class added to the modal panel (alongside `zs-auth-modal`). */
  className?: string;
  /** Extra content rendered below the iframe (e.g. a "sign up" link). */
  children?: ReactNode;
}

/**
 * An overlay dialog that hosts the platform's first-party login. Opening the
 * modal launches `signInWithOAuth({provider:'password'})`, which — on the
 * same-site console with `immersive` enabled — drives the in-page iframe
 * embedding `auth.zeroship.ai/login` (the Stripe-Elements model: the credential
 * is typed into the auth-origin frame, never readable by console JS). The modal
 * provides the close affordance: dismissing it cancels the relay race
 * (`popup_closed`) and the SDK tears down the iframe (§8). The "Continue with
 * Google" button spawns a federated popup, fired synchronously in the click
 * handler so the browser does not block it. Themeable via `className` (panel) +
 * the `zs-auth-*` classes.
 */
export function AuthModal(props: AuthModalProps): ReactNode {
  const { open = true, onClose, onSuccess, title, hideOAuth, className, children } = props;
  const { signInWithOAuth } = useAuth();
  // The provider-owned mount controller the client reads at flow time. The
  // immersive iframe mounts INTO `host` (this modal's slot) and is cancelled by
  // resolving `cancelled` (§4.1, §8). Absent ⇒ no <AuthProvider> ancestor wired
  // it; the iframe then falls back to the headless overlay (still functional).
  const modalMount = useContext(ModalMountContext);
  useModalStyles();

  // Callback ref: register THIS modal's host slot synchronously as the element
  // attaches (before the launch effect runs), so the client's `iframeMount`
  // resolves it the moment the flow calls `createIframe`. Clearing on detach
  // restores the overlay fallback for any later modal-less flow.
  const setFrameHost = useCallback(
    (el: HTMLDivElement | null) => {
      if (modalMount) modalMount.current.host = el;
    },
    [modalMount],
  );

  // Per-open user-cancel deferred. `onClose`/unmount resolve it → the in-flight
  // `runIframe` rejects `popup_closed` and tears the frame down (§8). Held in a
  // ref so the close button can resolve it synchronously.
  const cancelResolveRef = useRef<(() => void) | null>(null);
  const resolveCancel = useCallback(() => {
    cancelResolveRef.current?.();
    cancelResolveRef.current = null;
  }, []);

  // Launch the immersive password flow once per open. The SDK mounts the
  // cross-origin login iframe (its `createIframe`) INTO the host slot above;
  // when the relay settles it resolves SIGNED_IN and tears the iframe down. A
  // `popup_closed` (user dismissed) is swallowed here — the error surfaces via
  // useAuth().error only for real failures.
  const launchedRef = useRef(false);
  useEffect(() => {
    if (!open) {
      launchedRef.current = false;
      return;
    }
    if (launchedRef.current) return;
    launchedRef.current = true;
    // Arm a fresh cancel signal for this open and publish it to the controller
    // BEFORE launching (the client snapshots `cancelled` inside `createIframe`).
    const cancelled = new Promise<void>((resolve) => {
      cancelResolveRef.current = resolve;
    });
    if (modalMount) modalMount.current.cancelled = cancelled;
    signInWithOAuth({ provider: "password" })
      .then(() => onSuccess?.())
      .catch(() => {
        // popup_closed (dismissed) / surfaced via useAuth().error otherwise.
      });
    // On close/unmount: resolve the cancel signal (rejects the in-flight flow)
    // and detach the controller so a later modal-less flow uses the overlay.
    return () => {
      resolveCancel();
      if (modalMount) {
        modalMount.current.cancelled = undefined;
        modalMount.current.host = null;
      }
    };
  }, [open, signInWithOAuth, onSuccess, modalMount, resolveCancel]);

  if (!open) return null;

  const onGoogle = () => {
    // Inside the gesture so the client opens the popup synchronously (unblocked).
    signInWithOAuth({ provider: "google" }).catch(() => {
      // Errors surface via useAuth().error; nothing to do here.
    });
  };

  // Dismissal: resolve the in-flight cancel signal first (so `runIframe` rejects
  // `popup_closed` immediately) THEN notify the caller to close the modal.
  const dismiss = () => {
    resolveCancel();
    onClose?.();
  };

  const panel = createElement(
    "div",
    {
      className: className ? `zs-auth-modal ${className}` : "zs-auth-modal",
      role: "dialog",
      "aria-modal": true,
      // Stop a click inside the panel from bubbling to the backdrop's onClose.
      onClick: (e: React.MouseEvent) => e.stopPropagation(),
    },
    createElement(
      "div",
      { key: "header", className: "zs-auth-modal-header" },
      createElement("h2", { key: "title", className: "zs-auth-title" }, title ?? "Sign in"),
      createElement(
        "button",
        {
          key: "close",
          type: "button",
          className: "zs-auth-close",
          "aria-label": "Close",
          "data-testid": "auth-modal-close",
          onClick: dismiss,
        },
        "×",
      ),
    ),
    // The cross-origin login iframe is mounted by the SDK (its `createIframe`)
    // INTO this host slot — it fills the slot, sitting BELOW the modal's own
    // chrome (title + close button), so the close affordance stays reachable
    // (§8/§10.5). The callback ref registers the slot with the client.
    createElement("div", {
      key: "frame",
      className: "zs-auth-frame",
      "data-testid": "auth-iframe-host",
      ref: setFrameHost,
    }),
    hideOAuth
      ? null
      : createElement("div", { key: "divider", className: "zs-auth-divider" }, "or"),
    hideOAuth
      ? null
      : createElement(
          "button",
          { key: "google", type: "button", className: "zs-auth-oauth", onClick: onGoogle },
          "Continue with Google",
        ),
    children ?? null,
  );

  return createElement(
    "div",
    {
      className: "zs-auth-modal-backdrop",
      onClick: dismiss,
    },
    panel,
  );
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
