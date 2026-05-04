// ─── Terms — public legal stub (`/legal/terms`) ─────────────────
//
// Per spec §5.5. V1 placeholder until counsel-reviewed copy lands.

import { Link } from "react-router-dom";
import { PublicNav } from "../components/PublicNav";

export function Terms() {
  return (
    <div className="min-h-screen bg-paper" data-testid="terms-page">
      <PublicNav />

      <main className="mx-auto px-6 pt-14 pb-24" style={{ maxWidth: 720 }}>
        <section className="reveal mb-10">
          <div className="label-uc mb-4 flex items-center gap-2">
            <span className="inline-block h-px w-3.5 bg-ink" aria-hidden="true" />
            Legal · Terms
          </div>
          <h1
            className="font-serif font-medium leading-[0.98] -tracking-[0.02em] mb-3"
            style={{ fontSize: "clamp(40px, 5vw, 60px)" }}
          >
            Terms of <em className="italic text-tomato">service</em>.
          </h1>
          <p className="font-mono text-[12px] text-ink-soft tracking-wide">
            Last updated: 2026-05-01 · Placeholder text — counsel-reviewed
            version pending.
          </p>
        </section>

        <article className="font-serif text-[16px] leading-[1.65] text-ink space-y-6 mb-14">
          <Section title="Your account">
            <p>
              You're responsible for your account, the projects you ship
              under it, and the people you invite. Don't share credentials.
              Don't use someone else's email.
            </p>
          </Section>

          <Section title="Your code">
            <p>
              You own what you build. The agents wrote it; you ship it; it's
              yours. You can export the bundle (<code className="font-mono text-[14px] not-italic">.zship</code>) at any time
              and run it elsewhere — no lock-in. We get a non-exclusive
              licence only to run, store, and serve it on your behalf.
            </p>
          </Section>

          <Section title="What you can't ship">
            <p>
              No malware, phishing, CSAM, content that violates anyone's
              rights, or anything illegal in the jurisdictions where the
              app is reachable. We will take down anything that crosses
              these lines and may terminate the responsible account.
            </p>
          </Section>

          <Section title="Payments and the 15&nbsp;% share">
            <p>
              When you charge for your app, Stripe handles the transaction.
              We retain 15&nbsp;% of the post-Stripe-fees revenue as our
              platform share. The remainder is yours, paid out to your
              connected Stripe account on Stripe's standard schedule.
            </p>
          </Section>

          <Section title="Service availability">
            <p>
              We aim for 99.95&nbsp;% uptime on Pro plans (see the SLA in
              your account dashboard). Free and Maker plans are best-effort.
              We schedule maintenance and announce material outages on{" "}
              <Link to="/changelog" className="text-tomato">/changelog</Link>.
            </p>
          </Section>

          <Section title="Liability">
            <p>
              We provide the service "as is" to the extent permitted by law.
              Our liability is capped at the fees you paid us in the
              twelve months preceding the claim.
            </p>
          </Section>

          <Section title="Changes">
            <p>
              We'll email account holders 30 days before any material change
              to these terms. Continuing to use the service after the
              effective date constitutes acceptance.
            </p>
          </Section>

          <Section title="Contact">
            <p>
              <a href="mailto:legal@zeroship.dev" className="text-tomato">legal@zeroship.dev</a>.
            </p>
          </Section>
        </article>

        <hr className="hairline mb-10" />

        <footer className="font-sans text-[11px] uppercase tracking-[0.2em] text-pencil flex flex-wrap gap-x-7 gap-y-3 items-center">
          <Link to="/" className="hover:text-ink transition-colors" style={{ textDecoration: "none", color: "inherit" }}>Home</Link>
          <Link to="/legal/privacy" className="hover:text-ink transition-colors" style={{ textDecoration: "none", color: "inherit" }}>Privacy</Link>
          <Link to="/about" className="hover:text-ink transition-colors" style={{ textDecoration: "none", color: "inherit" }}>About</Link>
          <span className="ml-auto font-serif italic text-[12px] tracking-normal normal-case text-pencil">
            zeroship<span className="text-tomato">.</span> &copy; 2026
          </span>
        </footer>
      </main>
    </div>
  );
}

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div>
      <h3 className="font-serif italic font-medium text-[19px] -tracking-[0.01em] text-ink mb-2">
        {title}
      </h3>
      <div className="text-ink-soft">{children}</div>
    </div>
  );
}
