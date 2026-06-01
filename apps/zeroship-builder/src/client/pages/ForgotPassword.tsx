// ─── ForgotPassword — password recovery (UI stub) ────────────────
//
// Per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §6.3. The control plane doesn't yet expose a
// /auth/forgot-password endpoint, so this
// page renders as a UI stub: it accepts an email, "submits", and
// always shows the standard no-enumeration confirmation message.
// When the endpoint lands, replace `submit` with the real call and
// keep the same visible reply.
//
// Crystal: built over @zeroship/ui — a centered, elevated Card holds
// the Field + Input email row and the full-width submit Button; the
// post-submit reply is a live info Banner (announced once it appears,
// preserving the no-enumeration guarantee). Bespoke wordmark + footer
// chrome lives in the co-located ForgotPassword.css reading --zs-*.

import { useState, type FormEvent } from "react";
import { Link } from "react-router-dom";
import { Banner, Button, Card, Center, Field, Input, Stack } from "@zeroship/ui";
import "./ForgotPassword.css";

export default function ForgotPassword() {
  const [email, setEmail] = useState("");
  const [submitted, setSubmitted] = useState(false);

  function submit(e: FormEvent) {
    e.preventDefault();
    if (!email.trim()) return;
    // No backend yet — show the standard reply unconditionally.
    // Once /auth/forgot-password ships, call it here and ignore the
    // status to keep the no-enumeration guarantee.
    setSubmitted(true);
  }

  return (
    <Center minHeight="100dvh" className="fp-center">
      <Card
        variant="elevated"
        className="fp-card"
        data-testid="forgot-password-page"
      >
        <Stack gap={5}>
          <Link to="/" className="fp-wordmark">
            zeroship<span className="fp-wordmark__dot">.</span>
          </Link>

          <Stack gap={2}>
            <h1 className="fp-title">
              Forgot your <em className="fp-title__accent">password</em>?
            </h1>
            <p className="fp-lede">
              Tell us the email you signed up with — we'll send a reset link.
            </p>
          </Stack>

          {submitted ? (
            <Banner
              intent="info"
              live
              data-testid="forgot-password-confirmation"
            >
              If an account exists for <em className="fp-confirm__email">{email}</em>, we sent reset instructions.
            </Banner>
          ) : (
            <form onSubmit={submit} className="fp-form">
              <Stack gap={4}>
                <Field required>
                  <Field.Label>Email</Field.Label>
                  <Input
                    type="email"
                    autoComplete="email"
                    autoFocus
                    required
                    value={email}
                    onChange={(e) => setEmail(e.target.value)}
                    data-testid="forgot-password-email"
                  />
                </Field>
                <Button
                  type="submit"
                  variant="filled"
                  size="large"
                  disabled={!email.trim()}
                  className="fp-submit"
                  data-testid="forgot-password-submit"
                >
                  Send reset link
                </Button>
              </Stack>
            </form>
          )}

          <p className="fp-footer">
            Remembered it?{" "}
            <Link to="/login" className="fp-footer__link" data-testid="forgot-password-link-login">
              Sign in →
            </Link>
          </p>
        </Stack>
      </Card>
    </Center>
  );
}
