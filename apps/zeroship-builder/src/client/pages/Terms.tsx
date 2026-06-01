// ─── Terms — public legal stub (`/legal/terms`) ─────────────────
//
// Per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §5.5. V1 placeholder until counsel-reviewed copy lands.
//
// Crystal skin: PublicNav + a DS Container rendered as the page <main>,
// a PageHeader title band (eyebrow / title / last-updated description),
// and the legal prose laid out with the Stack primitive. Bespoke type +
// the footer caption row live in the co-located Terms.css (all --zs-*
// tokens). The public export, routing, links, and every data-testid are
// preserved exactly — only the presentation changed.

import type { ReactNode } from "react";
import { Link } from "react-router-dom";
import { Container, PageHeader, Separator, Stack } from "@zeroship/ui";
import { PublicNav } from "../components/PublicNav";
import "./Terms.css";

export function Terms() {
  return (
    <div className="zs-terms" data-testid="terms-page">
      <PublicNav />

      <Container asChild size="md" padX={6}>
        <main className="zs-terms__main">
          <PageHeader>
            <PageHeader.Text>
              <span className="zs-terms__eyebrow">
                <span className="zs-terms__eyebrow-rule" aria-hidden="true" />
                Legal · Terms
              </span>
              <PageHeader.Title>Terms of service.</PageHeader.Title>
              <PageHeader.Description className="zs-terms__updated">
                Last updated: 2026-05-01 · Placeholder text — counsel-reviewed
                version pending.
              </PageHeader.Description>
            </PageHeader.Text>
          </PageHeader>

          <Stack
            asChild
            gap={6}
            className="zs-terms__prose"
            style={{ marginBlockStart: "var(--zs-space-8)" }}
          >
            <article>
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
                  yours. You can export the bundle (<code>.zship</code>) at any
                  time and run it elsewhere — no lock-in. We get a non-exclusive
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
                  <Link to="/changelog" className="zs-terms__link">
                    /changelog
                  </Link>
                  .
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
                  <a href="mailto:legal@zeroship.dev" className="zs-terms__link">
                    legal@zeroship.dev
                  </a>
                  .
                </p>
              </Section>
            </article>
          </Stack>

          <Separator
            decorative
            style={{ marginBlock: "var(--zs-space-8) var(--zs-space-7)" }}
          />

          <footer className="zs-terms__footer">
            <Link to="/" className="zs-terms__footer-link">
              Home
            </Link>
            <Link to="/legal/privacy" className="zs-terms__footer-link">
              Privacy
            </Link>
            <Link to="/about" className="zs-terms__footer-link">
              About
            </Link>
            <span className="zs-terms__wordmark">
              zeroship<span className="zs-terms__wordmark-dot">.</span> &copy;
              2026
            </span>
          </footer>
        </main>
      </Container>
    </div>
  );
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <Stack gap={2}>
      <h3 className="zs-terms__section-title">{title}</h3>
      <div>{children}</div>
    </Stack>
  );
}
