// ─── Privacy — public legal stub (`/legal/privacy`) ─────────────
//
// Per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §5.5. V1 placeholder until counsel-reviewed copy lands.
// The shape is right — sections, plain language, last-updated date.
//
// Crystal skin: PublicNav + a width-constrained Container <main>, a DS
// PageHeader title band, and the legal prose in a Container/Stack. The
// bespoke chrome (page canvas, eyebrow rule, section + body type, links,
// footer row) lives in the co-located Privacy.css, all --zs-* tokens.
// Routing, the data-testid, and the section copy are preserved exactly —
// only the presentation changed.

import { Link } from "react-router-dom";
import { Container, PageHeader, Separator, Stack } from "@zeroship/ui";
import { PublicNav } from "../components/PublicNav";
import "./Privacy.css";

export function Privacy() {
  return (
    <div className="zs-privacy-page" data-testid="privacy-page">
      <PublicNav />

      <Container
        asChild
        size="md"
        padX={6}
        className="zs-privacy-page__main"
        style={{ "--container-max-width": "45rem" } as React.CSSProperties}
      >
        <main>
          <Stack gap={9}>
            <PageHeader>
              <PageHeader.Text>
                <span className="zs-privacy-page__eyebrow">
                  <span
                    className="zs-privacy-page__eyebrow-rule"
                    aria-hidden="true"
                  />
                  Legal · Privacy
                </span>
                <PageHeader.Title>
                  Privacy{" "}
                  <span className="zs-privacy-page__title-accent">policy</span>.
                </PageHeader.Title>
                <p className="zs-privacy-page__meta">
                  Last updated: 2026-05-01 · Placeholder text —
                  counsel-reviewed version pending.
                </p>
              </PageHeader.Text>
            </PageHeader>

            <Stack gap={7} asChild>
              <article>
                <Section title="What we collect">
                  <p className="zs-privacy-page__body">
                    We collect the minimum we need to run the platform: your
                    email and name on signup; your projects, deploys, and chat
                    history while using the service; standard server logs (IP,
                    user-agent, request paths) for security and debugging.
                  </p>
                </Section>

                <Section title="How we use it">
                  <p className="zs-privacy-page__body">
                    To run your account, deliver service-related email
                    (verifications, billing receipts), debug failures, and
                    aggregate (anonymised) usage to improve the platform. We do
                    not sell your data. We do not run third-party ad trackers.
                  </p>
                </Section>

                <Section title="Where it lives">
                  <p className="zs-privacy-page__body">
                    Your projects' code and bundles live in our object storage
                    (S3-compatible). Database rows live in PostgreSQL. We use
                    Stripe for payments — they receive only what they need to
                    process your transactions.
                  </p>
                </Section>

                <Section title="Your rights">
                  <p className="zs-privacy-page__body">
                    You can export your projects (as <code>.zship</code>{" "}
                    bundles) and your account data at any time. You can delete
                    your account; data is hard-deleted after a 30-day grace
                    window. EU residents have full GDPR rights — email{" "}
                    <a href="mailto:privacy@zeroship.dev">
                      privacy@zeroship.dev
                    </a>
                    .
                  </p>
                </Section>

                <Section title="Cookies">
                  <p className="zs-privacy-page__body">
                    One first-party session cookie. No third-party tracking
                    cookies. We use anonymised analytics that do not require
                    consent banners under GDPR.
                  </p>
                </Section>

                <Section title="Contact">
                  <p className="zs-privacy-page__body">
                    Questions?{" "}
                    <a href="mailto:privacy@zeroship.dev">
                      privacy@zeroship.dev
                    </a>
                    .
                  </p>
                </Section>
              </article>
            </Stack>

            <Stack gap={9}>
              <Separator />
              <footer className="zs-privacy-page__footer">
                <Link to="/" className="zs-privacy-page__footer-link">
                  Home
                </Link>
                <Link
                  to="/legal/terms"
                  className="zs-privacy-page__footer-link"
                >
                  Terms
                </Link>
                <Link to="/about" className="zs-privacy-page__footer-link">
                  About
                </Link>
                <span className="zs-privacy-page__footer-mark">
                  zeroship
                  <span className="zs-privacy-page__footer-mark-dot">.</span>{" "}
                  &copy; 2026
                </span>
              </footer>
            </Stack>
          </Stack>
        </main>
      </Container>
    </div>
  );
}

function Section({
  title,
  children,
}: {
  title: string;
  children: React.ReactNode;
}) {
  return (
    <Stack gap={2}>
      <h3 className="zs-privacy-page__section-title">{title}</h3>
      {children}
    </Stack>
  );
}
