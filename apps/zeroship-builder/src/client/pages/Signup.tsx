// ─── Signup — crystal auth (immersive iframe + Google popup) ─────
//
// Account creation shares the platform BFF with Login. The same
// identity-only session + HttpOnly cookie is established two ways:
//   • Email/password — IMMERSIVE: the SDK `<AuthModal>` hosts a
//     cross-origin, same-site iframe embedding `auth.zeroship.ai`'s real
//     login form (`signInWithOAuth({provider:'password'})`). The dev/IdP
//     provider does register-or-sign-in inside that auth-origin frame; the
//     credential never enters console JS (SOP). On SIGNED_IN the provider
//     snapshot flips and the `useEffect` below routes into the onboarding
//     intent flow. (In dev the auth provider is same-origin, so the framed
//     login is fillable in-frame.)
//   • Google — the federated `SignInButton(provider="google")` popup.
//
// Presentation rides the @zeroship/ui crystal surface: a `Center`d
// elevated `Card` carries the brand mark, a title + description, an
// optional danger `Banner`, the Google launcher, an "or" `Separator`,
// the "Sign up with email" button that opens the immersive `<AuthModal>`
// (themed via the co-located .css `zs-auth-*` modal-chrome hooks), and a
// footer link back to /login.

import { useEffect, useState } from "react";
import { Link, useLocation, useNavigate } from "react-router-dom";
import { Banner, Button, Card, Center, Separator, Stack } from "@zeroship/ui";
import { useAuth } from "../auth/AuthContext";
import { AuthModal, SignInButton, useAuth as useSdkAuth } from "@zeroship/auth/react";
import "./Signup.css";

export default function Signup() {
  const location = useLocation();
  const navigate = useNavigate();
  const { user, loading } = useAuth();
  const { error } = useSdkAuth();
  const [modalOpen, setModalOpen] = useState(false);

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
    <Center minHeight="100dvh" className="signup-shell">
      <Card variant="elevated" className="signup-card" data-testid="signup-page">
        <Stack gap={5}>
          <Link to="/" className="signup-brand">
            zeroship<span className="signup-brand__dot">.</span>
          </Link>

          <Stack gap={2}>
            <h1 className="signup-title">
              Make <em className="signup-title__accent">something</em>.
            </h1>
            <p className="signup-lede">
              Sign up to start a project. We'll save your work the moment you
              describe it.
            </p>
          </Stack>

          {oauthError && (
            <Banner intent="danger" live data-testid="signup-oauth-error">
              sign-up: {oauthError}
            </Banner>
          )}

          <Stack gap={3}>
            <SignInButton
              data-testid="signup-google"
              className="signup-oauth-btn"
              provider="google"
            >
              <GoogleG />
              Sign up with Google
            </SignInButton>

            <div className="signup-or" role="presentation">
              <Separator className="signup-or__line" />
              <span className="signup-or__label">or</span>
              <Separator className="signup-or__line" />
            </div>

            {/* Immersive email signup. Clicking opens the SDK `<AuthModal>`,
                which launches `signInWithOAuth({provider:'password'})` →
                the cross-origin, same-site iframe embedding the real
                `auth.zeroship.ai` login. The dev/IdP provider does
                register-or-sign-in; the credential never enters console JS
                (SOP). On SIGNED_IN the provider snapshot flips and the
                `useEffect` above routes to /onboarding/intent. */}
            <Button
              variant="filled"
              size="large"
              className="signup-email-trigger"
              data-testid="signup-email-trigger"
              onClick={() => setModalOpen(true)}
            >
              Sign up with email
            </Button>
          </Stack>

          <p className="signup-footer">
            Already have one?{" "}
            <Link to="/login" className="signup-footer__link">
              Sign in →
            </Link>
          </p>
        </Stack>
      </Card>

      {/* The immersive login modal hosts the cross-origin auth iframe; we
          keep the federated Google launcher on the page (above) so the
          modal hides its built-in one (`hideOAuth`). onSuccess relies on
          the SIGNED_IN snapshot flip → the `useEffect` navigates. */}
      <AuthModal
        open={modalOpen}
        onClose={() => setModalOpen(false)}
        title="Create your account"
        hideOAuth
        className="signup-auth-modal"
      />
    </Center>
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
    <svg viewBox="0 0 24 24" className="signup-google-g" aria-hidden="true">
      <path fill="#EA4335" d="M12 11v3.4h5.4c-.2 1.3-1.5 3.7-5.4 3.7-3.2 0-5.9-2.7-5.9-5.9 0-3.3 2.7-5.9 5.9-5.9 1.8 0 3.1.8 3.8 1.5l2.6-2.5C16.7 3.7 14.6 2.8 12 2.8 6.9 2.8 2.8 6.9 2.8 12s4.1 9.2 9.2 9.2c5.3 0 8.8-3.7 8.8-9 0-.6-.1-1.1-.2-1.5H12z" />
    </svg>
  );
}
