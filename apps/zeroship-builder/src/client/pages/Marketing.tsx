// ─── Marketing — public landing page (`/`) ──────────────────────
//
// Per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §5.1.
// Crystal migration: the editorial single-column page is rebuilt on the
// @zeroship/ui `sections/` layer — Hero, FeatureGrid (how-it-works steps +
// the differentiators band), a bespoke template gallery band, StatsBand,
// Cta, and Footer — wrapped by the migrated PublicNav. Each section is a
// full-bleed band that owns its own inner Container measure + vertical
// rhythm; the page only stacks them and paints the backdrop. Bespoke bits
// (the page backdrop + the template gallery band) live in the co-located
// Marketing.css, all --zs-* tokens.
//
// The page tells two headline stories — secure from day one, and a
// self-evolving loop that keeps improving the app after it ships — in
// plain, non-technical language. No money mechanics anywhere.
//
// PUBLIC — no AuthGuard. The "Begin" CTA shoots the visitor at /new
// (the wizard) which itself is public; createApp is the gate that
// triggers a 401 → /login redirect when the visitor isn't signed in.

import { Link, useNavigate } from "react-router-dom";
import {
  Button,
  Container,
  Cta,
  FeatureGrid,
  Footer,
  Grid,
  Hero,
  Icon,
  Stack,
  StatsBand,
} from "@zeroship/ui";
import { Package, RefreshCw, ShieldCheck, Zap } from "lucide-react";
import { PublicNav } from "../components/PublicNav";
import { TemplateCard } from "../components/TemplateCard";
import { TEMPLATES } from "../lib/templates";
import "./Marketing.css";

const FEATURED_TEMPLATE_SLUGS = [
  "recipe-journal",
  "tip-jar",
  "portfolio",
  "subscription",
];

export function Marketing() {
  const navigate = useNavigate();
  const featured = FEATURED_TEMPLATE_SLUGS
    .map((slug) => TEMPLATES.find((t) => t.slug === slug))
    .filter((t): t is (typeof TEMPLATES)[number] => Boolean(t));

  return (
    <div className="zs-marketing" data-testid="marketing-page">
      <PublicNav />

      <main className="zs-marketing__main">
        {/* ─── Hero ─────────────────────────────────────────────── */}
        <Hero
          align="start"
          backdrop
          eyebrow="Vol. I · Issue 05 · 2026"
          title="Anyone can ship software."
          description="Describe the thing you want in a plain sentence — a recipe journal, a tip jar, a booking page, anything — and a fleet of agents writes it, ships it to a real, live URL, and keeps making it better. Sign-in, privacy, and the careful safety work are built in from the very first line, so your data and your people are protected without you ever having to think about it."
          actions={
            <>
              <Button
                variant="filled"
                size="large"
                onClick={() => navigate("/new")}
                data-testid="marketing-begin"
              >
                Begin your project
              </Button>
              <Button asChild variant="plain" size="large">
                <Link to="/templates">See an example →</Link>
              </Button>
            </>
          }
        />

        {/* ─── How it works ─────────────────────────────────────── */}
        <FeatureGrid
          align="start"
          columns={4}
          eyebrow="How it works"
          title="From a sentence to a living app"
          description="Usually live in under a minute — and it keeps growing from there."
          features={STEPS.map((s, i) => ({
            id: s.title,
            title: (
              <>
                <span className="zs-marketing__step-num">
                  № {String(i + 1).padStart(2, "0")}
                </span>
                {s.title}
              </>
            ),
            description: s.body,
          }))}
        />

        {/* ─── Featured templates ───────────────────────────────── */}
        <section
          className="zs-marketing__templates"
          data-testid="marketing-templates"
          aria-labelledby="marketing-templates-title"
        >
          <Container size="lg">
            <Stack gap={6}>
              <div className="zs-marketing__templates-head">
                <Stack gap={2}>
                  <p className="zs-section-eyebrow">Starting points</p>
                  <h2
                    id="marketing-templates-title"
                    className="zs-marketing__templates-title"
                  >
                    Or begin from a template.
                  </h2>
                </Stack>
                <Button asChild variant="plain" size="small">
                  <Link to="/templates">See all →</Link>
                </Button>
              </div>

              <Grid columns={{ sm: 2, lg: 4 }} gap={5}>
                {featured.map((t) => (
                  <TemplateCard key={t.slug} template={t} />
                ))}
              </Grid>
            </Stack>
          </Container>
        </section>

        {/* ─── Why it holds — the two headline ideas + supporting ─ */}
        <FeatureGrid
          align="start"
          columns={2}
          tone="muted"
          eyebrow="Why it holds"
          title="Two promises, built in — not bolted on."
          description="Your app keeps improving long after it ships, and it's safe and private the whole way through."
          features={DIFFERENTIATORS.map((d) => ({
            id: d.title,
            icon: <Icon as={d.icon} size="lg" />,
            title: d.title,
            description: d.body,
          }))}
        />

        {/* ─── Proof bar — safety / speed / ownership (no money) ── */}
        <StatsBand
          align="center"
          eyebrow="In short"
          title="Safe, fast, and yours."
          stats={[
            {
              id: "secure",
              value: "Day one",
              label: "Locked down and private — never bolted on",
            },
            {
              id: "fast",
              value: "< 1 min",
              label: "From a sentence to a live URL",
            },
            {
              id: "yours",
              value: "100%",
              label: "Your code, yours to export",
            },
          ]}
        />

        {/* ─── Final CTA ────────────────────────────────────────── */}
        <Cta
          tone="accent"
          eyebrow="Begin"
          title="Tell us what you'd like to make."
          description="Start with one sentence and watch the agents turn it into something real, safe, and live — usually in under a minute. And they don't stop when it ships: change anything, anytime, just by asking."
          actions={
            <>
              <Button
                variant="filled"
                size="large"
                onClick={() => navigate("/new")}
              >
                Begin your project
              </Button>
              <Button asChild variant="plain" size="large">
                <Link to="/templates">Browse templates →</Link>
              </Button>
            </>
          }
        />

        {/* ─── Footer ───────────────────────────────────────────── */}
        <Footer
          brand={
            <Link to="/" className="zs-marketing__footer-brand">
              zeroship<span className="zs-marketing__footer-dot">.</span>
            </Link>
          }
          description="Anyone can ship software."
          columns={FOOTER_COLUMNS}
          copyright={<>&copy; 2026 zeroship</>}
        />
      </main>
    </div>
  );
}

const STEPS: { title: string; body: string }[] = [
  {
    title: "Say it in a sentence.",
    body:
      "Describe your idea the way you'd tell a friend — “a members-only journal for my supper club” is plenty to begin.",
  },
  {
    title: "Answer a question or two.",
    body:
      "A short, friendly back-and-forth — who signs in, what stays private, how it should feel. Answer what you like, skip the rest.",
  },
  {
    title: "The agents build it — and check each other.",
    body:
      "One agent writes it, another reads it back and fixes what's off, and it's locked down before it ever goes live. You watch it take shape.",
  },
  {
    title: "It goes live, then keeps growing.",
    body:
      "A real URL with everything included — and the agents stay on watch, catching trouble and refining it as it runs. Want it changed? Just say so.",
  },
];

const DIFFERENTIATORS: {
  title: string;
  body: string;
  icon: typeof ShieldCheck;
}[] = [
  {
    title: "Safe from the very first line.",
    icon: ShieldCheck,
    body:
      "Sign-in, private data, and the quiet, critical safety work are part of your app from the moment it begins — never bolted on later, never something you have to remember. Every app is locked down and yours alone, before it ever reaches the world.",
  },
  {
    title: "A living loop, not a one-time build.",
    icon: RefreshCw,
    body:
      "Your app isn't frozen the day it ships. The agents stay on the job — watching how it runs, catching the small things before you'd ever notice, and refining the rough edges — and when you want something changed, you just say so.",
  },
  {
    title: "Live in under a minute.",
    icon: Zap,
    body:
      "From a single sentence to a working link you can open and share, usually before your coffee cools. No accounts to wire up, no servers to rent, nothing to configure — you describe, it appears.",
  },
  {
    title: "Your code is yours.",
    icon: Package,
    body:
      "Everything the agents write belongs to you, and you can take it with you whenever you like. Database, sign-in, payments, and scaling are all included — we host it and keep it running, but you're never locked in.",
  },
];

const FOOTER_COLUMNS = [
  {
    id: "product",
    title: "Product",
    links: [
      { label: "Pricing", href: "/pricing" },
      { label: "Templates", href: "/templates" },
      { label: "Skills", href: "/skills" },
    ],
  },
  {
    id: "company",
    title: "Company",
    links: [
      { label: "About", href: "/about" },
      { label: "Changelog", href: "/changelog" },
    ],
  },
  {
    id: "legal",
    title: "Legal",
    links: [
      { label: "Privacy", href: "/legal/privacy" },
      { label: "Terms", href: "/legal/terms" },
    ],
  },
];
