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
  useId,
  useMemo,
  useRef,
  useState,
  type ButtonHTMLAttributes,
  type FormEvent,
  type FormHTMLAttributes,
  type ReactNode,
} from "react";

import { createAuthClient, type AuthClient } from "./client";
import {
  AuthError,
  type AuthChangeEvent,
  type AuthClientOptions,
  type CredentialsInput,
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

  /** Interactive popup sign-in for federated providers (gesture-safe when called from a click handler). */
  signInWithOAuth(opts?: SignInOptions): Promise<void>;
  /** In-page password sign-in — POSTs `{email, password}` same-origin; NO popup. */
  signInWithCredentials(input: CredentialsInput): Promise<void>;
  /** Local (default) or global sign-out. */
  signOut(opts?: SignOutOptions): Promise<void>;
  /** Step-up: re-consent additional scopes via interactive popup (server-side grant; no token to the browser). */
  requestScopes(scopes: string[]): Promise<void>;
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

  // Bound methods. `void`-returning wrappers (signInWithOAuth /
  // signInWithCredentials / signOut) surface their own errors into `state.error`
  // so a fire-and-forget onClick caller doesn't leave an unhandled rejection.
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

  const signInWithCredentials = useCallback(
    async (input: CredentialsInput): Promise<void> => {
      try {
        await client.signInWithCredentials(input);
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
      signInWithCredentials,
      signOut,
      requestScopes,
      hasScope,
    }),
    [
      state,
      signInWithOAuth,
      signInWithCredentials,
      signOut,
      requestScopes,
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

// ── in-page credential sign-in (no popup) ────────────────────────────────────

// The SDK can't depend on @zeroship/ui (it's framework-agnostic), so the form
// ships unstyled-but-structured: stable class hooks (`zs-auth-*`) + a tiny
// scoped stylesheet a consumer fully overrides by re-declaring those classes,
// or replaces wholesale via `className` on the root. The builder themes these
// with its crystal tokens; a bare consumer still gets a usable, accessible form.

/** Minimal, override-friendly default styling for the credential form/modal. */
const FORM_STYLE = `
.zs-auth-form{display:flex;flex-direction:column;gap:.75rem;width:100%}
.zs-auth-field{display:flex;flex-direction:column;gap:.25rem}
.zs-auth-field label{font-size:.875rem;font-weight:500}
.zs-auth-field input{padding:.5rem .625rem;border:1px solid currentColor;border-radius:.375rem;font:inherit}
.zs-auth-error{color:#b00020;font-size:.875rem}
.zs-auth-submit{padding:.5rem .75rem;border-radius:.375rem;cursor:pointer;font:inherit}
.zs-auth-submit[disabled]{opacity:.6;cursor:progress}
.zs-auth-modal-backdrop{position:fixed;inset:0;display:flex;align-items:center;justify-content:center;background:rgba(0,0,0,.45)}
.zs-auth-modal{background:#fff;color:#111;padding:1.5rem;border-radius:.75rem;max-width:22rem;width:100%;display:flex;flex-direction:column;gap:1rem}
.zs-auth-divider{display:flex;align-items:center;gap:.5rem;font-size:.75rem;opacity:.7}
.zs-auth-divider::before,.zs-auth-divider::after{content:"";flex:1;height:1px;background:currentColor;opacity:.3}
.zs-auth-oauth{padding:.5rem .75rem;border-radius:.375rem;cursor:pointer;font:inherit;width:100%}
`;

/** Inject the default stylesheet once per document (no-op when already present / no DOM). */
function useFormStyles(): void {
  useEffect(() => {
    if (typeof document === "undefined") return;
    const ID = "zs-auth-form-styles";
    if (document.getElementById(ID)) return;
    const el = document.createElement("style");
    el.id = ID;
    el.textContent = FORM_STYLE;
    document.head.appendChild(el);
  }, []);
}

// Drop the native `onSubmit` (we own it) and `onError` (re-typed to an AuthError).
type FormBaseProps = Omit<FormHTMLAttributes<HTMLFormElement>, "onSubmit" | "onError">;

export interface SignInFormProps extends FormBaseProps {
  /** Called after a successful in-page sign-in (SIGNED_IN already emitted). */
  onSuccess?: () => void;
  /** Called if `signInWithCredentials` rejects (in addition to the inline error). */
  onError?: (error: AuthError) => void;
  /** Submit-button label. Default `"Sign in"`. */
  submitLabel?: ReactNode;
  /** Label for the email field. Default `"Email"`. */
  emailLabel?: ReactNode;
  /** Label for the password field. Default `"Password"`. */
  passwordLabel?: ReactNode;
  /** `data-testid` for the email `<input>`. */
  emailTestId?: string;
  /** `data-testid` for the password `<input>`. */
  passwordTestId?: string;
  /** `data-testid` for the submit `<button>`. */
  submitTestId?: string;
  /** `data-testid` for the inline error `<p role="alert">` (rendered only on failure). */
  errorTestId?: string;
}

/**
 * In-page email + password sign-in form. Submitting calls
 * `useAuth().signInWithCredentials({email,password})` (a same-origin POST — NO
 * popup, NO window), shows an inline {@link AuthError} on failure, and invokes
 * `onSuccess` once SIGNED_IN settles. Themeable: pass `className` for the root
 * `<form>` (added alongside the `zs-auth-form` hook) or override the `zs-auth-*`
 * classes; any extra `<form>` props pass through.
 */
export function SignInForm(props: SignInFormProps): ReactNode {
  const {
    onSuccess,
    onError,
    submitLabel,
    emailLabel,
    passwordLabel,
    emailTestId,
    passwordTestId,
    submitTestId,
    errorTestId,
    className,
    ...formProps
  } = props;
  const { signInWithCredentials } = useAuth();
  useFormStyles();

  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState<AuthError | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const mountedRef = useRef(true);
  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  const baseId = useId();
  const emailId = `${baseId}-email`;
  const passwordId = `${baseId}-password`;

  const onSubmit = useCallback(
    (e: FormEvent<HTMLFormElement>) => {
      e.preventDefault();
      if (submitting) return;
      setError(null);
      setSubmitting(true);
      signInWithCredentials({ email, password })
        .then(() => {
          if (!mountedRef.current) return;
          setSubmitting(false);
          onSuccess?.();
        })
        .catch((err: unknown) => {
          const ae = toAuthError(err);
          if (mountedRef.current) {
            setError(ae);
            setSubmitting(false);
          }
          onError?.(ae);
        });
    },
    [signInWithCredentials, email, password, submitting, onSuccess, onError],
  );

  return createElement(
    "form",
    {
      ...formProps,
      className: className ? `zs-auth-form ${className}` : "zs-auth-form",
      onSubmit,
      noValidate: true,
    },
    createElement(
      "div",
      { className: "zs-auth-field", key: "email" },
      createElement("label", { htmlFor: emailId }, emailLabel ?? "Email"),
      createElement("input", {
        id: emailId,
        type: "email",
        name: "email",
        autoComplete: "email",
        required: true,
        value: email,
        disabled: submitting,
        "data-testid": emailTestId,
        onChange: (ev: React.ChangeEvent<HTMLInputElement>) => setEmail(ev.target.value),
      }),
    ),
    createElement(
      "div",
      { className: "zs-auth-field", key: "password" },
      createElement("label", { htmlFor: passwordId }, passwordLabel ?? "Password"),
      createElement("input", {
        id: passwordId,
        type: "password",
        name: "password",
        autoComplete: "current-password",
        required: true,
        value: password,
        disabled: submitting,
        "data-testid": passwordTestId,
        onChange: (ev: React.ChangeEvent<HTMLInputElement>) => setPassword(ev.target.value),
      }),
    ),
    error
      ? createElement(
          "p",
          { className: "zs-auth-error", role: "alert", key: "error", "data-testid": errorTestId },
          error.message,
        )
      : null,
    createElement(
      "button",
      {
        type: "submit",
        className: "zs-auth-submit",
        disabled: submitting,
        key: "submit",
        "data-testid": submitTestId,
      },
      submitting ? "Signing in…" : (submitLabel ?? "Sign in"),
    ),
  );
}

export interface AuthModalProps {
  /** Render the overlay only when true. Default `true` (caller can keep it mounted-but-hidden). */
  open?: boolean;
  /** Called when the backdrop is clicked (caller closes the modal). */
  onClose?: () => void;
  /** Forwarded to the inner {@link SignInForm}; fired after a successful in-page sign-in. */
  onSuccess?: () => void;
  /** Heading rendered above the form. Default `"Sign in"`. */
  title?: ReactNode;
  /** Hide the "Continue with Google" federated button. Default `false`. */
  hideOAuth?: boolean;
  /** Class added to the modal panel (alongside `zs-auth-modal`). */
  className?: string;
  /** Extra content rendered below the form (e.g. a "sign up" link). */
  children?: ReactNode;
}

/**
 * An overlay dialog wrapping {@link SignInForm} plus a "Continue with Google"
 * button. The password path is fully in-page (no `window.open`); ONLY the
 * federated button spawns a popup (`signInWithOAuth({provider:"google"})`),
 * fired synchronously in the click handler so the browser does not block it.
 * Themeable via `className` (panel) + the `zs-auth-*` classes.
 */
export function AuthModal(props: AuthModalProps): ReactNode {
  const { open = true, onClose, onSuccess, title, hideOAuth, className, children } = props;
  const { signInWithOAuth } = useAuth();
  useFormStyles();

  if (!open) return null;

  const onGoogle = () => {
    // Inside the gesture so the client opens the popup synchronously (unblocked).
    signInWithOAuth({ provider: "google" }).catch(() => {
      // Errors surface via useAuth().error; nothing to do here.
    });
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
    createElement("h2", { key: "title", className: "zs-auth-title" }, title ?? "Sign in"),
    createElement(SignInForm, { key: "form", onSuccess }),
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
      onClick: () => onClose?.(),
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
  type CredentialsInput,
  type Session,
  type SignInOptions,
  type SignOutOptions,
  type User,
} from "./types";
