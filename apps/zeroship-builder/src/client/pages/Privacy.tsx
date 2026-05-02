// ─── Privacy — public legal stub (`/legal/privacy`) ─────────────
//
// Per spec §5.5. V1 placeholder until counsel-reviewed copy lands.
// The shape is right — sections, plain language, last-updated date.

import { Link } from "react-router-dom";
import { PublicNav } from "../components/PublicNav";

export function Privacy() {
  return (
    <div className="min-h-screen bg-paper" data-testid="privacy-page">
      <PublicNav />

      <main className="mx-auto px-6 pt-14 pb-24" style={{ maxWidth: 720 }}>
        <section className="reveal mb-10">
          <div className="label-uc mb-4 flex items-center gap-2">
            <span className="inline-block h-px w-3.5 bg-ink" aria-hidden="true" />
            Legal · Privacy
          </div>
          <h1
            className="font-serif font-medium leading-[0.98] -tracking-[0.02em] mb-3"
            style={{ fontSize: "clamp(40px, 5vw, 60px)" }}
          >
            Privacy <em className="italic text-tomato">policy</em>.
          </h1>
          <p className="font-mono text-[12px] text-ink-soft tracking-wide">
            Last updated: 2026-05-01 · Placeholder text — counsel-reviewed
            version pending.
          </p>
        </section>

        <article className="font-serif text-[16px] leading-[1.65] text-ink space-y-6 mb-14">
          <Section title="What we collect">
            <p>
              We collect the minimum we need to run the platform: your email
              and name on signup; your projects, deploys, and chat history
              while using the service; standard server logs (IP, user-agent,
              request paths) for security and debugging.
            </p>
          </Section>

          <Section title="How we use it">
            <p>
              To run your account, deliver service-related email
              (verifications, billing receipts), debug failures, and
              aggregate (anonymised) usage to improve the platform. We do
              not sell your data. We do not run third-party ad trackers.
            </p>
          </Section>

          <Section title="Where it lives">
            <p>
              Your projects' code and bundles live in our object storage
              (S3-compatible). Database rows live in PostgreSQL. We use
              Stripe for payments — they receive only what they need to
              process your transactions.
            </p>
          </Section>

          <Section title="Your rights">
            <p>
              You can export your projects (as <code className="font-mono text-[14px] not-italic">.zsapp</code> bundles) and your account data at
              any time. You can delete your account; data is hard-deleted
              after a 30-day grace window. EU residents have full GDPR
              rights — email{" "}
              <a href="mailto:privacy@zeroship.dev" className="text-tomato">
                privacy@zeroship.dev
              </a>
              .
            </p>
          </Section>

          <Section title="Cookies">
            <p>
              One first-party session cookie. No third-party tracking
              cookies. We use anonymised analytics that do not require
              consent banners under GDPR.
            </p>
          </Section>

          <Section title="Contact">
            <p>
              Questions? <a href="mailto:privacy@zeroship.dev" className="text-tomato">privacy@zeroship.dev</a>.
            </p>
          </Section>
        </article>

        <hr className="hairline mb-10" />

        <footer className="font-sans text-[11px] uppercase tracking-[0.2em] text-pencil flex flex-wrap gap-x-7 gap-y-3 items-center">
          <Link to="/" className="hover:text-ink transition-colors" style={{ textDecoration: "none", color: "inherit" }}>Home</Link>
          <Link to="/legal/terms" className="hover:text-ink transition-colors" style={{ textDecoration: "none", color: "inherit" }}>Terms</Link>
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
