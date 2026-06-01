// ─── Marketing — public landing page (`/`) ──────────────────────
//
// Per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §5.1.
// Crystal migration: the editorial single-column page is rebuilt on the
// @zeroship/ui `sections/` layer — Hero, FeatureGrid, a bespoke template
// gallery band, StatsBand, Cta, and Footer — wrapped by the migrated
// PublicNav. Each section is a full-bleed band that owns its own inner
// Container measure + vertical rhythm; the page only stacks them and
// paints the backdrop. Bespoke bits (the page backdrop + the template
// gallery band) live in the co-located Marketing.css, all --zs-* tokens.
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
  Stack,
  StatsBand,
} from "@zeroship/ui";
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
          description={
            <>
              Describe the thing you want — a recipe journal, a tip jar, a
              booking page, anything — and a fleet of agents writes the code,
              deploys it, and runs it on a real, live URL.{" "}
              <strong>You keep what you make. The platform takes 15&nbsp;%.</strong>
            </>
          }
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
          title="From a sentence to a live URL"
          description="Usually under a minute, start to finish."
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

        {/* ─── Pricing proof bar ────────────────────────────────── */}
        <StatsBand
          align="center"
          tone="muted"
          eyebrow="What it costs"
          title="Free to start. 15% when your apps earn."
          description="No project fees. No per-seat pricing. Aligned incentives — the platform earns when you do."
          stats={[
            { id: "start", value: "$0", label: "To start building" },
            { id: "share", value: "15%", label: "Platform share, on revenue" },
            { id: "keep", value: "~$82", label: "Kept on $100 / mo earned" },
          ]}
        />

        {/* ─── Pricing teaser CTA ───────────────────────────────── */}
        <Cta
          tone="accent"
          eyebrow="When they earn"
          title="The platform earns when you do."
          description="A creator who makes $100 / mo keeps about $82 after Stripe and the platform share."
          actions={
            <>
              <Button asChild variant="filled" size="large">
                <Link to="/pricing">See pricing →</Link>
              </Button>
              <Button
                variant="plain"
                size="large"
                onClick={() => navigate("/new")}
              >
                Begin your project
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
    title: "Type what you want.",
    body:
      "A sentence is plenty. “A recipe journal for my supper club” — that's enough to begin.",
  },
  {
    title: "A short conversation.",
    body:
      "The wizard asks one or two clarifying questions — design, sign-in, payments. Answer or skip.",
  },
  {
    title: "The agents build it.",
    body:
      "Builder writes the code. Critic reviews it. Reviewer gates each commit. You watch it happen.",
  },
  {
    title: "It ships.",
    body:
      "A real URL on the zeroship runtime — database, auth, payments, scaling, all included. Iterate by chatting.",
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
