// ─── ForgotPassword — password recovery (UI stub) ────────────────
//
// Per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §6.3. The control plane doesn't yet expose a
// /auth/forgot-password endpoint, so this
// page renders as a UI stub: it accepts an email, "submits", and
// always shows the standard no-enumeration confirmation message.
// When the endpoint lands, replace `submit` with the real call and
// keep the same visible reply.

import { useState, type FormEvent } from "react";
import { Link } from "react-router-dom";
import { StampButton } from "../components/StampButton";

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
    <div className="min-h-screen flex items-center justify-center px-4 sm:px-5">
      <div className="w-[440px] max-w-full bg-white border border-rule px-6 sm:px-10 py-8 sm:py-10 reveal" data-testid="forgot-password-page">
        <Link to="/" className="font-serif italic text-[20px] font-medium text-ink hover:opacity-80 mb-8 inline-block" style={{ textDecoration: "none" }}>
          zeroship<span className="text-tomato">.</span>
        </Link>

        <h1 className="font-serif font-medium text-[36px] -tracking-[0.015em] leading-[1.05] mb-1.5">
          Forgot your <em className="italic text-tomato">password</em>?
        </h1>
        <p className="font-serif text-[14.5px] text-ink-soft mb-7 leading-[1.55]">
          Tell us the email you signed up with — we'll send a reset link.
        </p>

        {submitted ? (
          <div
            className="px-3 py-3 border border-rule bg-paper-2 font-serif text-[14px] text-ink leading-[1.55]"
            data-testid="forgot-password-confirmation"
          >
            If an account exists for <em className="italic">{email}</em>, we sent reset instructions.
          </div>
        ) : (
          <form onSubmit={submit} className="space-y-4">
            <label className="block">
              <span className="block label-uc mb-1.5">Email</span>
              <input
                type="email"
                autoComplete="email"
                autoFocus
                required
                value={email}
                onChange={(e) => setEmail(e.target.value)}
                data-testid="forgot-password-email"
                className="w-full px-3 py-2.5 border border-rule bg-white font-serif text-[15px] text-ink outline-none focus:border-ink"
              />
            </label>
            <div className="pt-2">
              <StampButton
                type="submit"
                disabled={!email.trim()}
                className="w-full"
                data-testid="forgot-password-submit"
                noArrow
              >
                Send reset link
              </StampButton>
            </div>
          </form>
        )}

        <div className="font-serif text-[14px] text-ink-soft text-center mt-6">
          Remembered it?{" "}
          <Link to="/login" className="text-tomato hover:opacity-80" style={{ textDecoration: "none" }} data-testid="forgot-password-link-login">
            Sign in →
          </Link>
        </div>
      </div>
    </div>
  );
}
