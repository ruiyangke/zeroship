// ─── Login — crystal auth (BFF: in-page password + Google popup) ──
//
// The console authenticates creators through the platform BFF
// (`@zeroship/auth/react`). Two paths share one identity-only session +
// HttpOnly cookie:
//   • Email/password — IN-PAGE via the SDK `<SignInForm>`, which POSTs
//     `{email,password}` same-origin to `/__zeroship/auth/password`
//     (`signInWithCredentials`); NO popup, NO window. On success the SDK
//     emits SIGNED_IN and the provider publishes the authenticated
//     snapshot — the `useEffect` below then bounces to `/home`.
//   • Google — the federated `SignInButton(provider="google")` popup,
//     wrapped in a DS Button via `asChild` (the SDK button owns the
//     gesture click that opens the popup; the DS Button lends it crystal
//     chrome). Its error (popup_blocked / popup_closed) surfaces inline.
//
// Crystal: a centered Card (Center + Card) holds a Stack of the wordmark,
// headline, optional error Banner, the Google launcher, an "or" rule, and
// the in-page `<SignInForm>` (themed via the co-located Login.css, which
// re-declares the SDK's `zs-auth-*` hooks against `--zs-*` tokens). All
// type/lockup/form styling reads `--zs-*` tokens — no raw hex/px.

import { useEffect } from "react";
import { Link, useLocation, useNavigate } from "react-router-dom";
import { Banner, Button, Card, Center, Separator, Stack } from "@zeroship/ui";
import { useAuth } from "../auth/AuthContext";
import { SignInButton, SignInForm, useAuth as useSdkAuth } from "@zeroship/auth/react";
import "./Login.css";

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
    <Center className="zb-login" minHeight="100dvh">
      <Card variant="elevated" className="zb-login__card" data-testid="login-page">
        <Stack gap={5}>
          <Link to="/" className="zb-login__brand">
            zeroship<span className="zb-login__brand-dot">.</span>
          </Link>

          <Stack gap={2}>
            <h1 className="zb-login__title">
              Welcome <em>back</em>.
            </h1>
            <p className="zb-login__lede">
              Pick up where you left off — your projects are right where you saved them.
            </p>
          </Stack>

          {oauthError && (
            <Banner intent="danger" live data-testid="login-oauth-error">
              sign-in: {oauthError}
            </Banner>
          )}

          <Stack gap={3}>
            {/* Popup OAuth launcher. `SignInButton` calls signInWithOAuth
                INSIDE the click gesture so the browser does not block the
                popup; the DS Button lends it crystal chrome via asChild. */}
            <Button
              variant="gray"
              size="large"
              className="zb-login__button"
              startSlot={<GoogleG />}
              asChild
            >
              <SignInButton data-testid="login-google" provider="google">
                Continue with Google
              </SignInButton>
            </Button>

            <div className="zb-login__or" role="presentation">
              <Separator className="zb-login__or-line" />
              <span className="zb-login__or-label">or</span>
              <Separator className="zb-login__or-line" />
            </div>

            {/* In-page email + password — the SDK form POSTs same-origin to
                `/__zeroship/auth/password` (no popup). On SIGNED_IN the
                provider snapshot flips and the `useEffect` above navigates;
                the form shows its own inline AuthError on a bad pair. Themed
                via the `zb-login__form` class + the `zs-auth-*` overrides in
                Login.css. testids are preserved through the SDK form. */}
            <SignInForm
              className="zb-login__form"
              submitLabel="Sign in"
              emailTestId="login-email"
              passwordTestId="login-password"
              submitTestId="login-submit"
              errorTestId="login-error"
            />
          </Stack>

          <p className="zb-login__footer">
            No account?{" "}
            <Link to="/signup" className="zb-login__footer-link" data-testid="login-link-signup">
              Make one →
            </Link>
          </p>
        </Stack>
      </Card>
    </Center>
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
    <svg viewBox="0 0 24 24" className="zb-login__google-mark" aria-hidden="true">
      <path fill="#EA4335" d="M12 11v3.4h5.4c-.2 1.3-1.5 3.7-5.4 3.7-3.2 0-5.9-2.7-5.9-5.9 0-3.3 2.7-5.9 5.9-5.9 1.8 0 3.1.8 3.8 1.5l2.6-2.5C16.7 3.7 14.6 2.8 12 2.8 6.9 2.8 2.8 6.9 2.8 12s4.1 9.2 9.2 9.2c5.3 0 8.8-3.7 8.8-9 0-.6-.1-1.1-.2-1.5H12z" />
    </svg>
  );
}
