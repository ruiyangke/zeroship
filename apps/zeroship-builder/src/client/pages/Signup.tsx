// ─── Signup — atelier auth ──────────────────────────────────────

import { useState, type FormEvent } from "react";
import { Link, useLocation, useNavigate } from "react-router-dom";
import { useMutation } from "@tanstack/react-query";
import { register as apiRegister, googleStartUrl } from "../api/auth";
import { useAuth } from "../auth/AuthContext";
import { StampButton } from "../components/StampButton";

export default function Signup() {
  const location = useLocation();
  const navigate = useNavigate();
  const { refresh } = useAuth();
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [name, setName] = useState("");

  const params = new URLSearchParams(location.search);
  const returnTo = sanitizeReturn(params.get("return"));

  const mut = useMutation({
    mutationFn: () => apiRegister({ email, password, name }),
    onSuccess: async () => {
      await refresh();
      navigate(returnTo, { replace: true });
    },
  });

  function submit(e: FormEvent) {
    e.preventDefault();
    if (!email.trim() || !password || !name.trim()) return;
    mut.mutate();
  }

  return (
    <div className="min-h-screen flex items-center justify-center px-5">
      <div className="w-[440px] max-w-full bg-white border border-rule px-10 py-10 reveal" data-testid="signup-page">
        <Link to="/" className="font-serif italic text-[20px] font-medium text-ink hover:opacity-80 mb-8 inline-block" style={{ textDecoration: "none" }}>
          zeroship<span className="text-tomato">.</span>
        </Link>

        <h1 className="font-serif font-medium text-[36px] -tracking-[0.015em] leading-[1.05] mb-1.5">
          Make <em className="italic text-tomato">something</em>.
        </h1>
        <p className="font-serif text-[14.5px] text-ink-soft mb-7 leading-[1.55]">
          Sign up to start a project. We'll save your work the moment you describe it.
        </p>

        <form onSubmit={submit} className="space-y-4">
          <Field label="Email" type="email" autoComplete="email" autoFocus required value={email} onChange={setEmail} testId="signup-email" />
          <Field
            label="Choose a password"
            type="password"
            autoComplete="new-password"
            required
            value={password}
            onChange={setPassword}
            testId="signup-password"
            help="At least 8 characters"
          />
          <Field
            label="What should we call you?"
            type="text"
            required
            value={name}
            onChange={setName}
            testId="signup-name"
            help="We'll only show this on projects you publish"
          />
          <div className="pt-2">
            <StampButton
              type="submit"
              loading={mut.isPending}
              disabled={mut.isPending || !email.trim() || !password || !name.trim()}
              className="w-full"
              data-testid="signup-submit"
              noArrow
            >
              {mut.isPending ? "Creating…" : "Create account"}
            </StampButton>
          </div>
          {mut.isError && (
            <div className="font-serif italic text-tomato text-[13px]" data-testid="signup-error">
              {mut.error.message}
            </div>
          )}
        </form>

        <div className="my-6 flex items-center gap-3 font-sans text-[10px] uppercase tracking-[0.2em] text-pencil">
          <span className="flex-1 h-px bg-rule" />or<span className="flex-1 h-px bg-rule" />
        </div>

        <a
          href={googleStartUrl(returnTo)}
          data-testid="signup-google"
          className="flex items-center justify-center gap-2 w-full h-10 border border-rule bg-white text-ink font-sans text-[11px] uppercase tracking-[0.18em] hover:border-ink transition-colors"
          style={{ textDecoration: "none" }}
        >
          <GoogleG />
          sign up with google
        </a>

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

function Field({
  label, type, autoComplete, autoFocus, required, value, onChange, testId, help,
}: {
  label: string; type: string; autoComplete?: string; autoFocus?: boolean; required?: boolean;
  value: string; onChange: (v: string) => void; testId?: string; help?: string;
}) {
  return (
    <label className="block">
      <span className="block label-uc mb-1.5">{label}</span>
      <input
        type={type}
        autoComplete={autoComplete}
        autoFocus={autoFocus}
        required={required}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        data-testid={testId}
        className="w-full px-3 py-2.5 border border-rule bg-white font-serif text-[15px] text-ink outline-none focus:border-ink"
      />
      {help && <span className="block mt-1 font-serif italic text-[12px] text-pencil">{help}</span>}
    </label>
  );
}

function sanitizeReturn(raw: string | null): string {
  // Post-signup default lands in the authed gallery at /home — `/` is
  // the public marketing page (per spec §5.1).
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
