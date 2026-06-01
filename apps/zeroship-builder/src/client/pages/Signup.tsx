// ─── Signup — atelier auth (BFF popup) ──────────────────────────
//
// Account creation is a hosted flow now: the same `@zeroship/auth`
// popup as Login, against the seeded per-app public PKCE client. The
// IdP's hosted-password screen handles register-or-sign-in; the
// retired bespoke `/auth/register` endpoint is gone. On first
// authentication we route into the onboarding intent flow.

import { useEffect } from "react";
import { Link, useLocation, useNavigate } from "react-router-dom";
import { useAuth } from "../auth/AuthContext";
import { SignInButton, useAuth as useSdkAuth } from "@zeroship/auth/react";

export default function Signup() {
  const location = useLocation();
  const navigate = useNavigate();
  const { user, loading } = useAuth();
  const { error } = useSdkAuth();

  const params = new URLSearchParams(location.search);
  const returnTo = sanitizeReturn(params.get("return"));
  const oauthError = params.get("error") ?? error?.message ?? null;

  useEffect(() => {
    if (!loading && user) {
      // First signup → run the onboarding intent flow per
      // `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §7.1.
      // A deeper `?return=…` (set by an AuthGuard bounce) is respected;
      // a bare /signup lands on the first-run survey.
      const dest = returnTo === "/home" ? "/onboarding/intent" : returnTo;
      navigate(dest, { replace: true });
    }
  }, [loading, user, returnTo, navigate]);

  return (
    <div className="min-h-screen flex items-center justify-center px-4 sm:px-5">
      <div className="w-[440px] max-w-full bg-white border border-rule px-6 sm:px-10 py-8 sm:py-10 reveal" data-testid="signup-page">
        <Link to="/" className="font-serif italic text-[20px] font-medium text-ink hover:opacity-80 mb-8 inline-block" style={{ textDecoration: "none" }}>
          zeroship<span className="text-tomato">.</span>
        </Link>

        <h1 className="font-serif font-medium text-[36px] -tracking-[0.015em] leading-[1.05] mb-1.5">
          Make <em className="italic text-tomato">something</em>.
        </h1>
        <p className="font-serif text-[14.5px] text-ink-soft mb-7 leading-[1.55]">
          Sign up to start a project. We'll save your work the moment you describe it.
        </p>

        {oauthError && (
          <div className="mb-4 px-3 py-2 border border-tomato bg-tomato/10 font-serif italic text-tomato text-[13px]" data-testid="signup-oauth-error">
            sign-up: {oauthError}
          </div>
        )}

        <SignInButton
          data-testid="signup-google"
          className="flex items-center justify-center gap-2 w-full h-10 border border-rule bg-white text-ink font-sans text-[11px] uppercase tracking-[0.18em] hover:border-ink transition-colors"
          provider="google"
        >
          <GoogleG />
          sign up with google
        </SignInButton>

        <div className="my-6 flex items-center gap-3 font-sans text-[10px] uppercase tracking-[0.2em] text-pencil">
          <span className="flex-1 h-px bg-rule" />or<span className="flex-1 h-px bg-rule" />
        </div>

        <SignInButton
          provider="password"
          data-testid="signup-submit"
          className="flex items-center justify-center gap-2 w-full h-10 bg-ink text-paper font-sans text-[11px] uppercase tracking-[0.18em] hover:opacity-90 transition-opacity"
        >
          Sign up with email
        </SignInButton>

        <div className="font-serif text-[14px] text-ink-soft text-center mt-6">
          Already have one?{" "}
          <Link to="/login" className="text-tomato hover:opacity-80" style={{ textDecoration: "none" }}>
            Sign in →
          </Link>
        </div>
      </div>
    </div>
  );
}

function sanitizeReturn(raw: string | null): string {
  // Post-signup default lands in the authed gallery at /home — `/` is
  // the public marketing page (per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §5.1).
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
