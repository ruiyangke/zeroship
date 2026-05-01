// ─── Account — profile + plan + sessions + 2FA + sign out ──────
//
// Per spec §6.5. V1 sections:
//   · Identity     — name + email (read-only; no edit endpoint yet).
//   · Plan         — free plan + usage + upgrade CTA.
//   · Sessions     — list of active sessions (deferred · ISS-10).
//   · Two-factor   — TOTP enrollment (deferred · ISS-11).
//   · Delete       — wipe the account (deferred · ISS-12).
//   · Sign out     — calls AuthContext.logout, redirects to /login.

import { useNavigate } from "react-router-dom";
import { useAuth } from "../auth/AuthContext";
import { PageFrame } from "../components/PageFrame";
import { GhostButton } from "../components/GhostButton";
import { StampButton } from "../components/StampButton";

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

        <div className="bg-paper-2 border border-rule px-7 py-6 grid items-center gap-6 mb-8" style={{ gridTemplateColumns: "2fr 1fr" }}>
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

        <DeferredSection
          title="Sessions"
          helper="Where you're signed in — log out remote devices anytime."
          issue="ISS-10"
          testId="account-sessions"
        />

        <DeferredSection
          title="Two-factor auth"
          helper="Add a second step (TOTP) to keep your projects safe."
          issue="ISS-11"
          testId="account-2fa"
        />

        <DeferredSection
          title="Delete account"
          helper="Wipe your projects, sessions, and identity for good."
          issue="ISS-12"
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

function Section({
  title, helper, children, lastSection,
}: {
  title: React.ReactNode; helper: React.ReactNode; children: React.ReactNode; lastSection?: boolean;
}) {
  return (
    <div className={"grid gap-12 py-6 " + (lastSection ? "" : "border-b border-rule")} style={{ gridTemplateColumns: "280px 1fr" }}>
      <div>
        <h3 className="font-serif italic font-medium text-[22px] mb-1.5">{title}</h3>
        {helper && <p className="font-serif text-[13.5px] text-ink-soft leading-[1.5]">{helper}</p>}
      </div>
      <div>{children}</div>
    </div>
  );
}

function DeferredSection({
  title, helper, issue, testId, tone,
}: {
  title: string; helper: string; issue: string; testId: string; tone?: "danger";
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
        Coming soon — see <code className="font-mono not-italic text-[12px]">ISSUES.md</code> {issue}.
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
