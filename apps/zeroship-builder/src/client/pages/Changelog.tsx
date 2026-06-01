// ─── Changelog — public chronology (`/changelog`) ───────────────
//
// Per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §5.5. A static, editorial list of recent changes. Entries
// are dated and described in the same voice as the app. New entries
// go on top.
//
// Crystal skin: PublicNav + a constrained Container holding a PageHeader
// band, then the entries as a Stack of DS Cards. Each entry is a Card
// whose body is a DescriptionList — the date/tag meta as the term column
// and the title/body as the detail. Bespoke type + chrome live in the
// co-located Changelog.css (all --zs-* tokens). The Entry shape, the
// ENTRIES data, the public `Changelog` export, routing, and every
// data-testid are preserved exactly — only the presentation changed.

import { Link } from "react-router-dom";
import {
  Card,
  Container,
  DescriptionList,
  PageHeader,
  Stack,
  Tag,
} from "@zeroship/ui";
import { PublicNav } from "../components/PublicNav";
import "./Changelog.css";

interface Entry {
  date: string;
  title: string;
  body: string;
  tag?: "feature" | "fix" | "polish";
}

const ENTRIES: Entry[] = [
  {
    date: "2026-05-01",
    title: "Public surfaces, end to end.",
    tag: "feature",
    body:
      "Marketing landing, pricing, skill catalogue, public templates, about, and legal stubs all ship together. An unauthed visitor can wander the whole storefront before signing up.",
  },
  {
    date: "2026-04-30",
    title: "Workspace canvases mount.",
    tag: "feature",
    body:
      "The project workspace gets its full canvas set — Preview, Code, Env, Settings — wired into the spine. Pills swap canvases without a route change.",
  },
  {
    date: "2026-04-29",
    title: "Critic loop in generation.",
    tag: "feature",
    body:
      "The Builder ⇄ Critic loop now runs on every code-gen turn. Critic gates the change; Reviewer enforces hard gates pre-deploy. The chat receipt shows iteration count and tokens spent.",
  },
  {
    date: "2026-04-28",
    title: "Creation flow.",
    tag: "feature",
    body:
      "The wizard ships. Type an idea on /, answer one or two clarifying questions, and the agents start coding. Templates seed the brief; the wizard skips ahead when the prompt is concrete enough.",
  },
  {
    date: "2026-04-27",
    title: "Vite plugin, static-only mode.",
    tag: "polish",
    body:
      "The vite-plugin gains a static-only build for SSG deploys, plus a virtual:zeroship/client-manifest module for SSR hydration. Smaller bundles, simpler deploys.",
  },
  {
    date: "2026-04-26",
    title: "Auth surfaces (login, signup, account).",
    tag: "feature",
    body:
      "Email + password, Google OAuth, forgot-password (no enumeration), account page with sessions / 2FA / delete stubs flagged as deferred.",
  },
];

export function Changelog() {
  return (
    <div className="zs-changelog" data-testid="changelog-page">
      <PublicNav />

      <Container size="sm" asChild>
        <main className="zs-changelog__main">
          <PageHeader>
            <PageHeader.Text>
              <div className="zs-changelog__eyebrow">
                <span
                  className="zs-changelog__eyebrow-rule"
                  aria-hidden="true"
                />
                Changelog
              </div>
              <PageHeader.Title>
                Latest changes
                <span className="zs-changelog__accent">.</span>
              </PageHeader.Title>
              <PageHeader.Description>
                What's new, what's fixed, what's polished. New entries go on
                top.
              </PageHeader.Description>
            </PageHeader.Text>
          </PageHeader>

          <Stack gap={4} asChild>
            <ol
              className="zs-changelog__entries"
              data-testid="changelog-entries"
            >
              {ENTRIES.map((e) => (
                <Card asChild key={e.date + e.title} variant="outline">
                  <li>
                    <Card.Content>
                      <DescriptionList orientation="horizontal">
                        <DescriptionList.Item>
                          <DescriptionList.Term>
                            <Stack gap={2} align="start">
                              <span className="zs-changelog__date">
                                {e.date}
                              </span>
                              {e.tag && <Tag size="sm">{e.tag}</Tag>}
                            </Stack>
                          </DescriptionList.Term>
                          <DescriptionList.Detail>
                            <Stack gap={2}>
                              <h3 className="zs-changelog__entry-title">
                                {e.title}
                              </h3>
                              <p className="zs-changelog__entry-body">
                                {e.body}
                              </p>
                            </Stack>
                          </DescriptionList.Detail>
                        </DescriptionList.Item>
                      </DescriptionList>
                    </Card.Content>
                  </li>
                </Card>
              ))}
            </ol>
          </Stack>

          <hr className="zs-changelog__rule" />

          <footer className="zs-changelog__footer">
            <Link to="/" className="zs-changelog__footer-link">
              Home
            </Link>
            <Link to="/pricing" className="zs-changelog__footer-link">
              Pricing
            </Link>
            <Link to="/templates" className="zs-changelog__footer-link">
              Templates
            </Link>
            <Link to="/about" className="zs-changelog__footer-link">
              About
            </Link>
            <span className="zs-changelog__footer-wordmark">
              zeroship<span className="zs-changelog__accent">.</span> &copy;
              2026
            </span>
          </footer>
        </main>
      </Container>
    </div>
  );
}
