// ─── Login — atelier auth (BFF popup) ───────────────────────────
//
// The console authenticates creators through the platform BFF popup
// flow (`@zeroship/auth/react`): clicking sign-in opens the gateway's
// `/__zeroship/auth/authorize` popup against the seeded per-app public PKCE
// client; on success the gateway sets the HttpOnly session cookie and
// the SDK publishes the authenticated snapshot. There is NO password
// form here anymore — the retired bespoke `/auth/{login,register}`
// endpoints are gone (design
// docs/superpowers/specs/2026-05-30-console-as-regular-app-design.md).

import { useEffect } from "react";
import { Link, useLocation, useNavigate } from "react-router-dom";
import { useAuth } from "../auth/AuthContext";
import { SignInButton, useAuth as useSdkAuth } from "@zeroship/auth/react";

export default function Login() {
  const location = useLocation();
  const navigate = useNavigate();
  const { user, loading } = useAuth();
  // The raw SDK hook exposes the popup error (popup_blocked / popup_closed)
  // for inline display; the adapter intentionally hides it.
  const { error } = useSdkAuth();

  const params = new URLSearchParams(location.search);
  const returnTo = sanitizeReturn(params.get("return"));
  const oauthError = params.get("error") ?? error?.message ?? null;

  // Once the popup completes the SDK flips the snapshot to authenticated;
  // bounce to the requested destination.
  useEffect(() => {
    if (!loading && user) navigate(returnTo, { replace: true });
  }, [loading, user, returnTo, navigate]);

  return (
    <div className="min-h-screen flex items-center justify-center px-4 sm:px-5">
      <div className="w-[440px] max-w-full bg-white border border-rule px-6 sm:px-10 py-8 sm:py-10 reveal" data-testid="login-page">
        <Link to="/" className="font-serif italic text-[20px] font-medium text-ink hover:opacity-80 mb-8 inline-block" style={{ textDecoration: "none" }}>
          zeroship<span className="text-tomato">.</span>
        </Link>

        <h1 className="font-serif font-medium text-[36px] -tracking-[0.015em] leading-[1.05] mb-1.5">
          Welcome <em className="italic text-tomato">back</em>.
        </h1>
        <p className="font-serif text-[14.5px] text-ink-soft mb-7 leading-[1.55]">
          Pick up where you left off — your projects are right where you saved them.
        </p>

        {oauthError && (
          <div className="mb-4 px-3 py-2 border border-tomato bg-tomato/10 font-serif italic text-tomato text-[13px]" data-testid="login-oauth-error">
            sign-in: {oauthError}
          </div>
        )}

        {/* Popup OAuth launcher. `SignInButton` calls signInWithOAuth INSIDE
            the click gesture so the browser does not block the popup. */}
        <SignInButton
          data-testid="login-google"
          className="flex items-center justify-center gap-2 w-full h-10 border border-rule bg-white text-ink font-sans text-[11px] uppercase tracking-[0.18em] hover:border-ink transition-colors"
          provider="google"
        >
          <GoogleG />
          continue with google
        </SignInButton>

        <div className="my-6 flex items-center gap-3 font-sans text-[10px] uppercase tracking-[0.2em] text-pencil">
          <span className="flex-1 h-px bg-rule" />or<span className="flex-1 h-px bg-rule" />
        </div>

        {/* Hosted-password sign-in — Phase-1 popup flow (provider=password). */}
        <SignInButton
          provider="password"
          data-testid="login-submit"
          className="flex items-center justify-center gap-2 w-full h-10 mt-3 bg-ink text-paper font-sans text-[11px] uppercase tracking-[0.18em] hover:opacity-90 transition-opacity"
        >
          Sign in with email
        </SignInButton>

        <div className="font-serif text-[14px] text-ink-soft text-center mt-6">
          No account?{" "}
          <Link to="/signup" className="text-tomato hover:opacity-80" style={{ textDecoration: "none" }} data-testid="login-link-signup">
            Make one →
          </Link>
        </div>
      </div>
    </div>
  );
}

function sanitizeReturn(raw: string | null): string {
  // Post-login default lands in the authed gallery at /home — `/` is
  // the public marketing page now (per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §5.1).
  if (!raw) return "/home";
  if (raw.startsWith("//") || raw.includes("://") || !raw.startsWith("/")) return "/home";
  return raw;
}

function GoogleG() {
  return (
    <svg viewBox="0 0 24 24" className="size-3.5" aria-hidden="true">
      <path fill="#EA4335" d="M12 11v3.4h5.4c-.2 1.3-1.5 3.7-5.4 3.7-3.2 0-5.9-2.7-5.9-5.9 0-3.3 2.7-5.9 5.9-5.9 1.8 0 3.1.8 3.8 1.5l2.6-2.5C16.7 3.7 14.6 2.8 12 2.8 6.9 2.8 2.8 6.9 2.8 12s4.1 9.2 9.2 9.2c5.3 0 8.8-3.7 8.8-9 0-.6-.1-1.1-.2-1.5H12z" />
    </svg>
  );
}
