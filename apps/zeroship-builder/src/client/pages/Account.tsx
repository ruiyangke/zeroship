// ─── Account — profile + plan + sessions + 2FA + sign out ──────
//
// Per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §6.5. Current sections:
//   · Identity     — name + email (read-only; no edit endpoint yet).
//   · Plan         — free plan + usage + upgrade CTA.
//   · Sessions     — list of active sessions (stubbed for now).
//   · Two-factor   — TOTP enrollment (not wired yet).
//   · Delete       — wipe the account (not wired yet).
//   · Sign out     — calls AuthContext.logout, redirects to /login.

import { useEffect, useMemo, useState } from "react";
import { useNavigate } from "react-router-dom";
import { useAuth } from "../auth/AuthContext";
import { PageFrame } from "../components/PageFrame";
import { GhostButton } from "../components/GhostButton";
import { StampButton } from "../components/StampButton";

const SESSION_START_KEY = "zeroship_session_started_at";

export function Account({ onLogout }: { onLogout?: () => void }) {
  const { user, logout } = useAuth();
  const navigate = useNavigate();
  const display = user?.name || user?.email || "—";
  const firstWord = display.split(/\s+/)[0];
  const rest = display.split(/\s+/).slice(1).join(" ");

  async function handleLogout() {
    await logout();
    onLogout?.();
    navigate("/login", { replace: true });
  }

  return (
    <PageFrame
      crumb={[{ label: "studio", to: "/home" }, { label: "account" }]}
      maxWidth={760}
      showMarginalia={false}
    >
      <section className="reveal" data-testid="account-page">
        <h1 className="font-serif font-medium text-[56px] leading-[0.98] -tracking-[0.02em] mb-1">
          {firstWord} {rest && <em className="italic text-tomato">{rest}</em>}
        </h1>
        <div className="font-serif text-[16px] text-ink-soft mb-9">
          <em className="italic">{user?.email ?? "—"}</em>
        </div>

        <div className="bg-paper-2 border border-rule px-5 sm:px-7 py-6 grid items-center gap-6 mb-8 grid-cols-1 sm:grid-cols-[2fr_1fr]">
          <div>
            <div className="font-serif italic text-[24px]"><em className="italic text-tomato">Free</em> plan</div>
            <div className="font-mono text-[12px] text-ink-soft mt-1.5">412 requests this week · 2 of 3 apps deployed</div>
            <div className="h-1.5 bg-paper-3 rounded-full overflow-hidden mt-2">
              <div className="bg-tomato h-full" style={{ width: "32%" }} />
            </div>
          </div>
          <div className="text-right">
            <StampButton>Upgrade</StampButton>
          </div>
        </div>

        <Section title="Identity" helper="Read-only for now — name + email come from your sign-up.">
          <Field label="Display name" value={user?.name ?? ""} readOnly testId="account-name" />
          <Field label="Email" value={user?.email ?? ""} readOnly testId="account-email" />
        </Section>

        <SessionsSection />

        <DeferredSection
          title="Two-factor auth"
          helper="Add a second step (TOTP) to keep your projects safe."
          pendingMessage="TOTP enrollment and recovery codes are not wired yet."
          testId="account-2fa"
        />

        <DeferredSection
          title="Delete account"
          helper="Wipe your projects, sessions, and identity for good."
          pendingMessage="Self-serve account deletion is not wired yet."
          testId="account-delete"
          tone="danger"
        />

        <Section title={<span className="text-tomato">Sign out</span>} helper="" lastSection>
          <GhostButton danger onClick={handleLogout} data-testid="account-logout">
            Sign out of zeroship
          </GhostButton>
        </Section>
      </section>
    </PageFrame>
  );
}

/**
 * Sessions block — V1 client-only view of "this browser's session".
 * Reads (or seeds) a localStorage timestamp for the start of the
 * current session and renders a single-row table per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §6.5. The
 * "sign out everywhere" affordance is wired to the same logout path
 * AuthContext exposes (in dev that's a no-op; in prod it'll revoke
 * once the control-plane sessions endpoint lands and the SDK switches to
 * the real list). Self-revoke shouldn't actually do anything in dev —
 * we surface it as "sign out" since that's all we can do client-side.
 */
function SessionsSection() {
  const { logout } = useAuth();
  const navigate = useNavigate();

  // Synthetic session start timestamp. Stable across page reloads so
  // "signed in 5m ago" stays accurate; resets on logout (cleared by
  // the logout handler so the next login mints a fresh start).
  const [startedAt, setStartedAt] = useState<string | null>(null);
  useEffect(() => {
    let v: string | null = null;
    try { v = localStorage.getItem(SESSION_START_KEY); } catch {}
    if (!v) {
      v = new Date().toISOString();
      try { localStorage.setItem(SESSION_START_KEY, v); } catch {}
    }
    setStartedAt(v);
  }, []);

  const ua = useMemo(() => {
    if (typeof navigator === "undefined") return "Browser";
    return shortUserAgent(navigator.userAgent);
  }, []);

  async function handleRevokeAll() {
    try { localStorage.removeItem(SESSION_START_KEY); } catch {}
    await logout();
    navigate("/login", { replace: true });
  }

  return (
    <Section title="Sessions" helper="Where you're signed in — V1 shows this browser only.">
      <div
        data-testid="account-sessions"
        className="border border-rule bg-paper-2/40 px-4 py-3"
      >
        <div className="grid items-baseline gap-3" style={{ gridTemplateColumns: "1fr auto" }}>
          <div>
            <div className="font-serif italic font-medium text-[15px] text-ink">
              {ua} · this browser
            </div>
            <div className="font-serif text-[13px] text-ink-soft">
              Signed in {startedAt ? relativeTime(startedAt) : "just now"}.
            </div>
          </div>
          <span
            data-testid="account-sessions-current-pill"
            className="font-sans text-[10px] uppercase tracking-[0.18em] text-ivy border border-ivy/40 rounded-full px-2 py-0.5"
          >
            current
          </span>
        </div>
        <div className="mt-3">
          <button
            type="button"
            onClick={handleRevokeAll}
            data-testid="account-sessions-revoke-all"
            className="font-serif italic text-[13px] text-tomato bg-transparent border-0 cursor-pointer hover:opacity-80"
          >
            Sign out everywhere →
          </button>
        </div>
        <div className="mt-2 font-serif italic text-[12px] text-pencil">
          Extra devices will appear here once the control plane exposes
          a real sessions list.
        </div>
      </div>
    </Section>
  );
}

function shortUserAgent(ua: string): string {
  // Tiny matcher — enough to label "Chrome on macOS" without dragging
  // in a real ua-parser dep. We only show this as flavour; if every
  // matcher misses we just show "Browser" rather than the raw string.
  const browser =
    /Firefox\/(\d+)/.test(ua) ? "Firefox" :
    /Edg\//.test(ua) ? "Edge" :
    /Chrome\//.test(ua) ? "Chrome" :
    /Safari\//.test(ua) ? "Safari" : "Browser";
  const os =
    /Mac OS X/.test(ua) ? "macOS" :
    /Windows NT/.test(ua) ? "Windows" :
    /X11.*Linux/.test(ua) ? "Linux" :
    /Android/.test(ua) ? "Android" :
    /iPhone|iPad/.test(ua) ? "iOS" : "";
  return os ? `${browser} on ${os}` : browser;
}

function relativeTime(iso: string): string {
  const t = new Date(iso).getTime();
  if (!Number.isFinite(t)) return "just now";
  const diff = Date.now() - t;
  if (diff < 60_000) return "just now";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)}m ago`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)}h ago`;
  return `${Math.floor(diff / 86_400_000)}d ago`;
}

function Section({
  title, helper, children, lastSection,
}: {
  title: React.ReactNode; helper: React.ReactNode; children: React.ReactNode; lastSection?: boolean;
}) {
  return (
    <div className={"grid gap-6 sm:gap-12 py-6 grid-cols-1 sm:grid-cols-[280px_1fr] " + (lastSection ? "" : "border-b border-rule")}>
      <div>
        <h3 className="font-serif italic font-medium text-[22px] mb-1.5">{title}</h3>
        {helper && <p className="font-serif text-[13.5px] text-ink-soft leading-[1.5]">{helper}</p>}
      </div>
      <div className="min-w-0">{children}</div>
    </div>
  );
}

function DeferredSection({
  title, helper, pendingMessage, testId, tone,
}: {
  title: string; helper: string; pendingMessage: string; testId: string; tone?: "danger";
}) {
  return (
    <Section
      title={tone === "danger" ? <span className="text-tomato">{title}</span> : title}
      helper={helper}
    >
      <div
        className="border border-dashed border-rule bg-paper-2 px-4 py-3 font-serif italic text-[13.5px] text-ink-soft leading-[1.55]"
        data-testid={testId}
      >
        Coming soon — {pendingMessage}
      </div>
    </Section>
  );
}

function Field({
  label, testId, ...rest
}: { label: string; testId?: string } & React.InputHTMLAttributes<HTMLInputElement>) {
  return (
    <label className="block mb-3.5">
      <span className="block label-uc mb-1">{label}</span>
      <input
        {...rest}
        data-testid={testId}
        className="w-full px-3 py-2.5 border border-rule bg-white font-serif text-[15px] text-ink outline-none focus:border-ink read-only:bg-paper-2"
      />
    </label>
  );
}
