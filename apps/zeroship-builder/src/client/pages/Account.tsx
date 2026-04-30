// ─── Account — profile + plan + usage + sign out ───────────────

import { useAuth } from "../auth/AuthContext";
import { PageFrame } from "../components/PageFrame";
import { GhostButton } from "../components/GhostButton";
import { StampButton } from "../components/StampButton";

export function Account({ onLogout }: { onLogout?: () => void }) {
  const { user, logout } = useAuth();
  const display = user?.name || user?.email || "—";
  const firstWord = display.split(/\s+/)[0];
  const rest = display.split(/\s+/).slice(1).join(" ");

  return (
    <PageFrame
      crumb={[{ label: "studio", to: "/" }, { label: "account" }]}
      maxWidth={760}
      showMarginalia={false}
    >
      <section className="reveal">
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

        <Section title="Profile" helper="Public-ish — shown on your projects' about pages if you opt in.">
          <Field label="Display name" defaultValue={user?.name ?? ""} />
          <Field label="Email" defaultValue={user?.email ?? ""} readOnly />
        </Section>

        <Section title="Billing" helper="Cards, invoices, payouts to you (when your apps earn).">
          <p className="font-serif italic text-ink-soft text-[14px] mb-2">No payment method yet.</p>
          <GhostButton>Add a card</GhostButton>
        </Section>

        <Section title={<span className="text-tomato">Sign out</span>} helper="" lastSection>
          <GhostButton danger onClick={() => { logout(); onLogout?.(); }} data-testid="account-logout">
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

function Field({ label, ...rest }: { label: string } & React.InputHTMLAttributes<HTMLInputElement>) {
  return (
    <label className="block mb-3.5">
      <span className="block label-uc mb-1">{label}</span>
      <input
        {...rest}
        className="w-full px-3 py-2.5 border border-rule bg-white font-serif text-[15px] text-ink outline-none focus:border-ink read-only:bg-paper-2"
      />
    </label>
  );
}
