// ─── About — public manifesto (`/about`) ────────────────────────
//
// Per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §5.5. Editorial single-column page. One or two paragraphs
// of voice — the philosophy, the why, the bet. Not a press kit.
//
// Crystal skin: PublicNav + a constrained Container holding a single
// Stack column — an eyebrow + display heading (PageHeader.Text), the
// manifesto prose in a Card, a Separator, and a wrapping Cluster
// footer. All bespoke type/treatment lives in the co-located sheet on
// --zs-* tokens; no Tailwind, no editorial tokens, no raw hex/px.

import { Link } from "react-router-dom";
import {
  Card,
  Cluster,
  Container,
  PageHeader,
  Separator,
  Stack,
} from "@zeroship/ui";
import { PublicNav } from "../components/PublicNav";
import "./About.css";

const FOOTER_LINKS: { to: string; label: string }[] = [
  { to: "/", label: "Home" },
  { to: "/pricing", label: "Pricing" },
  { to: "/changelog", label: "Changelog" },
  { to: "/legal/privacy", label: "Privacy" },
  { to: "/legal/terms", label: "Terms" },
];

export function About() {
  return (
    <div className="zs-about" data-testid="about-page">
      <PublicNav />

      <Container size="md" asChild>
        <main className="zs-about__main">
          <Stack gap={9}>
            <PageHeader>
              <PageHeader.Text className="zs-about__intro">
                <PageHeader.Description className="zs-about__eyebrow">
                  <span className="zs-about__eyebrow-rule" aria-hidden="true" />
                  Manifesto
                </PageHeader.Description>
                <PageHeader.Title className="zs-about__title">
                  Software, made by{" "}
                  <em className="zs-about__accent">anyone</em>.
                </PageHeader.Title>
              </PageHeader.Text>
            </PageHeader>

            <Card variant="ghost" size="lg" className="zs-about__prose">
              <Card.Content>
                <Stack gap={7} asChild>
                  <article>
                    <p>
                      For thirty years, shipping software has been a craft
                      fenced off by a paywall of patience: months of learning,
                      frameworks that change yearly, hosting bills that
                      compound, the slow grind of becoming fluent enough to be
                      dangerous. The people who needed software the most —
                      small operators, side-project dreamers, hobbyists with
                      one very good idea — almost always couldn't get there.
                    </p>

                    <p>
                      <em>zeroship</em> is the bet that this changes now.
                      Describe what you want in a sentence. A fleet of agents
                      writes the code, reads it back to catch what's off, locks
                      it down, ships it to a real, live URL, and keeps watching
                      how it runs — refining the rough edges long after launch
                      and reshaping the whole thing whenever you ask. Every app
                      is private and protected from the very first line, never
                      bolted on later. And the work is always yours: you can
                      export your code and run it anywhere, anytime, with
                      nothing holding it hostage.
                    </p>

                    <p>
                      The aesthetic is editorial because tools deserve more
                      than the chrome of dashboards. The infrastructure is
                      bespoke — built from the ground up for speed and
                      cost — because the maths has to work for the people who
                      build here. Running an app costs us almost nothing, so a
                      creator's idea can stand on its own from the start.
                    </p>

                    <p>
                      We're early. Some surfaces are stubs. The skill registry
                      isn't wired yet. There's a long list of "coming soon"
                      notes in our issue tracker, all of them honest. If you'd
                      like to help — or just see what the agents make of your
                      idea —{" "}
                      <Link to="/" className="zs-about__inline-link">
                        start a project
                      </Link>
                      . It usually takes under a minute.
                    </p>
                  </article>
                </Stack>
              </Card.Content>
            </Card>

            <Separator />

            <Cluster
              justify="between"
              align="center"
              gap={6}
              className="zs-about__footer"
            >
              <Cluster gap={6} className="zs-about__footer-links">
                {FOOTER_LINKS.map((l) => (
                  <Link key={l.to} to={l.to} className="zs-about__footer-link">
                    {l.label}
                  </Link>
                ))}
              </Cluster>
              <span className="zs-about__wordmark">
                zeroship<span className="zs-about__wordmark-dot">.</span> &copy;
                2026
              </span>
            </Cluster>
          </Stack>
        </main>
      </Container>
    </div>
  );
}
