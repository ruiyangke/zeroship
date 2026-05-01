// ─── Marketing — public landing page (`/`) ──────────────────────
//
// Per spec §5.1. Editorial single-column layout, max-width 760px.
// Hero with serif Fraunces headline + tomato-italic verb, sub-lede,
// inline "Begin your project" CTA. "How it works" section. Featured
// templates grid. Pricing teaser. Footer with public links.
//
// PUBLIC — no AuthGuard. The "Begin" CTA shoots the visitor at /new
// (the wizard) which itself is public; createApp is the gate that
// triggers a 401 → /login redirect when the visitor isn't signed in.

import { Link, useNavigate } from "react-router-dom";
import { PublicNav } from "../components/PublicNav";
import { StampButton } from "../components/StampButton";
import { TemplateCard } from "../components/TemplateCard";
import { TEMPLATES } from "../lib/templates";

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
    <div className="min-h-screen bg-paper" data-testid="marketing-page">
      <PublicNav />

      <main className="mx-auto px-6 pt-14 pb-24" style={{ maxWidth: 760 }}>
        {/* ─── Hero ─────────────────────────────────────────────── */}
        <section className="reveal mb-20">
          <div className="font-sans text-[10.5px] uppercase tracking-[0.22em] text-pencil mb-5">
            Vol. I · Issue 05 · 2026
          </div>
          <h1
            className="font-serif font-medium leading-[0.96] -tracking-[0.025em] mb-6"
            style={{ fontSize: "clamp(54px, 8vw, 96px)", fontVariationSettings: '"opsz" 144' }}
          >
            Anyone can{" "}
            <em className="italic text-tomato relative inline-block">
              ship
              <span
                aria-hidden
                className="absolute -bottom-0.5 left-0 right-0 h-2 bg-tomato opacity-20 rounded-sm"
                style={{ transform: "rotate(-1.2deg)" }}
              />
            </em>{" "}
            software.
          </h1>
          <p className="font-serif text-[20px] leading-[1.5] text-ink-soft max-w-[600px] mb-8">
            Describe the thing you want — a recipe journal, a tip jar, a
            booking page, anything — and a fleet of agents writes the code,
            deploys it, and runs it on a real, live URL. {" "}
            <strong className="text-ink font-medium">
              You keep what you make. The platform takes 15&nbsp;%.
            </strong>
          </p>

          <div className="flex items-baseline gap-5 flex-wrap">
            <StampButton
              onClick={() => navigate("/new")}
              data-testid="marketing-begin"
            >
              Begin your project
            </StampButton>
            <Link
              to="/templates"
              className="font-serif italic text-[15px] text-ink border-b border-rule hover:text-tomato hover:border-tomato py-1"
              style={{ textDecoration: "none" }}
            >
              See an example →
            </Link>
          </div>
        </section>

        <hr className="hairline mb-20" />

        {/* ─── How it works ─────────────────────────────────────── */}
        <section className="reveal mb-20">
          <div className="label-uc mb-3 flex items-center gap-2">
            <span className="inline-block h-px w-3.5 bg-ink" aria-hidden="true" />
            How it works
          </div>
          <h2
            className="font-serif font-medium leading-[1.0] -tracking-[0.018em] mb-10"
            style={{ fontSize: "clamp(36px, 5vw, 56px)" }}
          >
            From a sentence to a live URL — usually under a{" "}
            <em className="italic text-tomato">minute</em>.
          </h2>

          <ol className="m-0 p-0 list-none space-y-8">
            {STEPS.map((s, i) => (
              <li key={s.title} className="grid gap-5" style={{ gridTemplateColumns: "60px 1fr" }}>
                <div
                  className="font-serif italic text-[28px] text-tomato leading-none pt-2"
                  style={{ fontFeatureSettings: '"lnum" 1' }}
                >
                  №&nbsp;{String(i + 1).padStart(2, "0")}
                </div>
                <div>
                  <h3 className="font-serif font-medium text-[22px] -tracking-[0.012em] mb-1.5 leading-tight">
                    {s.title}
                  </h3>
                  <p className="font-serif italic text-[16px] text-ink-soft leading-[1.55] m-0">
                    {s.body}
                  </p>
                </div>
              </li>
            ))}
          </ol>
        </section>

        <hr className="hairline mb-20" />

        {/* ─── Featured templates ───────────────────────────────── */}
        <section className="reveal mb-20" data-testid="marketing-templates">
          <div className="flex items-baseline justify-between mb-5">
            <div>
              <div className="label-uc mb-2 flex items-center gap-2">
                <span className="inline-block h-px w-3.5 bg-ink" aria-hidden="true" />
                Starting points
              </div>
              <h2 className="font-serif font-medium text-[40px] -tracking-[0.018em] leading-[1.0]">
                Or begin from a <em className="italic text-tomato">template</em>.
              </h2>
            </div>
            <Link
              to="/templates"
              className="font-serif italic text-[14px] text-tomato hover:opacity-80"
              style={{ textDecoration: "none" }}
            >
              See all →
            </Link>
          </div>

          <div
            className="grid gap-5"
            style={{ gridTemplateColumns: "repeat(auto-fill, minmax(220px, 1fr))" }}
          >
            {featured.map((t) => (
              <TemplateCard key={t.slug} template={t} />
            ))}
          </div>
        </section>

        <hr className="hairline mb-20" />

        {/* ─── Pricing teaser ───────────────────────────────────── */}
        <section className="reveal mb-20">
          <div className="label-uc mb-3 flex items-center gap-2">
            <span className="inline-block h-px w-3.5 bg-ink" aria-hidden="true" />
            What it costs
          </div>
          <h2
            className="font-serif font-medium leading-[1.0] -tracking-[0.018em] mb-5"
            style={{ fontSize: "clamp(36px, 5vw, 56px)" }}
          >
            Free to start. <em className="italic text-tomato">15&nbsp;%</em> of what
            your apps earn — when they earn.
          </h2>
          <p className="font-serif text-[18px] leading-[1.55] text-ink-soft max-w-[580px] mb-6">
            No project fees. No per-seat pricing. Aligned incentives — the
            platform earns when you do. A creator who makes $100&nbsp;/mo
            keeps about <strong className="text-ink">$82</strong> after Stripe
            and the platform share.
          </p>
          <Link
            to="/pricing"
            className="inline-block font-serif italic text-[16px] text-ink border-b border-rule hover:text-tomato hover:border-tomato py-1"
            style={{ textDecoration: "none" }}
          >
            See pricing →
          </Link>
        </section>

        <hr className="hairline mb-12" />

        {/* ─── Footer ───────────────────────────────────────────── */}
        <footer className="font-sans text-[11px] uppercase tracking-[0.2em] text-pencil flex flex-wrap gap-x-7 gap-y-3 items-center">
          <Link to="/pricing" className="hover:text-ink transition-colors" style={{ textDecoration: "none", color: "inherit" }}>Pricing</Link>
          <Link to="/templates" className="hover:text-ink transition-colors" style={{ textDecoration: "none", color: "inherit" }}>Templates</Link>
          <Link to="/skills" className="hover:text-ink transition-colors" style={{ textDecoration: "none", color: "inherit" }}>Skills</Link>
          <Link to="/about" className="hover:text-ink transition-colors" style={{ textDecoration: "none", color: "inherit" }}>About</Link>
          <Link to="/changelog" className="hover:text-ink transition-colors" style={{ textDecoration: "none", color: "inherit" }}>Changelog</Link>
          <Link to="/legal/privacy" className="hover:text-ink transition-colors" style={{ textDecoration: "none", color: "inherit" }}>Privacy</Link>
          <Link to="/legal/terms" className="hover:text-ink transition-colors" style={{ textDecoration: "none", color: "inherit" }}>Terms</Link>
          <span className="ml-auto font-serif italic text-[12px] tracking-normal normal-case text-pencil">
            zeroship<span className="text-tomato">.</span> &copy; 2026
          </span>
        </footer>
      </main>
    </div>
  );
}

const STEPS: { title: string; body: string }[] = [
  {
    title: "Type what you want.",
    body:
      "A sentence is plenty. \u201CA recipe journal for my supper club\u201D — that's enough to begin.",
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
