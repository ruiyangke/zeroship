// ─── Pricing — public pricing surface (`/pricing`) ──────────────
//
// Per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §5.2. Three-tier card grid (Free / Maker / Pro), each
// with title, big price, tagline, bullet list, and a primary "Choose"
// button that routes to /signup?plan=<tier>.
//
// Top of page: the "Earn, keep most of it" lede explaining the
// 15 % platform share + Stripe fees, with a worked example mirroring
// the AGENTS.md revenue model.
//
// Crystal skin: PublicNav + a DS Container holds the page column. The
// tier band is composed from the same DS primitives the @zeroship/ui
// PricingTable section is built from — Container + Grid + Card — so the
// load-bearing per-card `data-testid` hooks (pricing-plan-*, the
// "Most chosen" flag, the responsive boundingBox checks) ride on real
// card elements. CTAs are DS Buttons (filled for the featured tier,
// plain elsewhere). All bespoke chrome lives in the co-located
// Pricing.css reading --zs-* tokens; no Tailwind, no editorial tokens,
// no raw hex/px. Props, exports, routing, navigation, and every
// data-testid are preserved exactly.

import { Link, useNavigate } from "react-router-dom";
import { Button, Card, Container, Grid } from "@zeroship/ui";
import { PublicNav } from "../components/PublicNav";
import "./Pricing.css";

interface Plan {
  key: "free" | "maker" | "pro";
  name: string;
  price: string;
  cadence: string;
  tagline: string;
  features: string[];
  cta: string;
  emphasised?: boolean;
}

const PLANS: Plan[] = [
  {
    key: "free",
    name: "Free",
    price: "$0",
    cadence: "forever",
    tagline: "Make a thing. See if it lives.",
    features: [
      "1 active project",
      "5,000 requests / month",
      "zeroship.app subdomain",
      "Maker mode (Ali) UX",
      "Basic agent fleet (Builder + Critic)",
    ],
    cta: "Start free",
  },
  {
    key: "maker",
    name: "Maker",
    price: "$19",
    cadence: "per month",
    tagline: "For the creator going public.",
    features: [
      "Unlimited projects",
      "100,000 requests / month",
      "Custom domain",
      "Priority build queue",
      "All canvases (Logs, Health, Data)",
      "PM + SRE agents enabled",
    ],
    cta: "Choose Maker",
    emphasised: true,
  },
  {
    key: "pro",
    name: "Pro",
    price: "$49",
    cadence: "per month",
    tagline: "For teams shipping serious things.",
    features: [
      "Everything in Maker",
      "Team seats (up to 5)",
      "Audit log + export",
      "99.95 % SLA",
      "No platform branding",
      "Dedicated email support",
    ],
    cta: "Choose Pro",
  },
];

export function Pricing() {
  const navigate = useNavigate();

  function choose(plan: Plan["key"]) {
    // No real Stripe yet; route to signup with the intent encoded in
    // the query so the post-signup experience can pick it up later.
    navigate(`/signup?plan=${plan}`);
  }

  return (
    <div className="zs-pricing-page" data-testid="pricing-page">
      <PublicNav />

      <Container size="lg" padX={6} asChild>
        <main className="zs-pricing-page__main">
          {/* ─── Lede ─────────────────────────────────────────────── */}
          <section className="zs-pricing-page__lede">
            <div className="zs-pricing-page__eyebrow">
              <span className="zs-pricing-page__eyebrow-rule" aria-hidden="true" />
              Pricing
            </div>
            <h1 className="zs-pricing-page__title">
              Earn, keep <em>most</em> of it.
            </h1>
            <p className="zs-pricing-page__lede-lead">
              Subscriptions to zeroship are simple — but the real arithmetic is
              that we earn a 15&nbsp;% share of what your apps make. Nothing
              until you do.
            </p>
            <p className="zs-pricing-page__lede-note">
              A worked example: a creator's app earns $100/mo. Stripe takes
              ≈$3.20. zeroship takes $15.00. The creator keeps{" "}
              <strong>$81.80</strong>.
            </p>
          </section>

          {/* ─── Plan cards ───────────────────────────────────────── */}
          <section className="zs-pricing-page__plans-section">
            <Grid
              minColWidth="16rem"
              gap={5}
              align="stretch"
              data-testid="pricing-plans"
            >
              {PLANS.map((p) => (
                <Card
                  key={p.key}
                  variant={p.emphasised ? "elevated" : "outline"}
                  data-testid={`pricing-plan-${p.key}`}
                  data-featured={p.emphasised ? "" : undefined}
                  className="zs-pricing-page__plan"
                >
                  {p.emphasised && (
                    <div className="zs-pricing-page__plan-badge">
                      Most chosen
                    </div>
                  )}
                  <h3 className="zs-pricing-page__plan-name">{p.name}</h3>
                  <p className="zs-pricing-page__plan-price">
                    <span className="zs-pricing-page__plan-amount">
                      {p.price}
                    </span>
                    <span className="zs-pricing-page__plan-cadence">
                      {p.cadence}
                    </span>
                  </p>
                  <p className="zs-pricing-page__plan-tagline">{p.tagline}</p>
                  <ul className="zs-pricing-page__plan-features">
                    {p.features.map((f) => (
                      <li key={f} className="zs-pricing-page__plan-feature">
                        {f}
                      </li>
                    ))}
                  </ul>
                  <div className="zs-pricing-page__plan-cta">
                    <Button
                      variant={p.emphasised ? "filled" : "plain"}
                      onClick={() => choose(p.key)}
                      data-testid={`pricing-cta-${p.key}`}
                    >
                      {p.emphasised ? p.cta : `${p.cta} →`}
                    </Button>
                  </div>
                </Card>
              ))}
            </Grid>
          </section>

          {/* ─── 15 % share band ──────────────────────────────────── */}
          <section className="zs-pricing-page__share">
            <div className="zs-pricing-page__eyebrow">
              <span className="zs-pricing-page__eyebrow-rule" aria-hidden="true" />
              The 15&nbsp;% share, plainly
            </div>
            <h2 className="zs-pricing-page__share-title">
              Aligned incentives, no platform fee until you sell.
            </h2>
            <p className="zs-pricing-page__share-body">
              zeroship makes money <em>only</em> when your apps make money. Run
              a free side project — the platform takes nothing. Charge for it —
              Stripe takes its standard fees and we take 15&nbsp;% of what's
              left. Infrastructure, agents, hosting, scaling, monitoring: all
              included.
            </p>
          </section>

          {/* ─── Enterprise note ──────────────────────────────────── */}
          <section className="zs-pricing-page__enterprise">
            <p className="zs-pricing-page__enterprise-note">
              Need an audit log, SSO, a custom contract, or volume pricing?{" "}
              <Link to="/about" className="zs-pricing-page__inline-link">
                Get in touch
              </Link>{" "}
              — we'll work something out.
            </p>
          </section>

          <hr className="zs-pricing-page__rule" />

          <footer className="zs-pricing-page__footer">
            <Link to="/" className="zs-pricing-page__footer-link">
              Home
            </Link>
            <Link to="/templates" className="zs-pricing-page__footer-link">
              Templates
            </Link>
            <Link to="/skills" className="zs-pricing-page__footer-link">
              Skills
            </Link>
            <Link to="/about" className="zs-pricing-page__footer-link">
              About
            </Link>
            <span className="zs-pricing-page__footer-mark">
              zeroship<span className="zs-pricing-page__footer-dot">.</span>{" "}
              &copy; 2026
            </span>
          </footer>
        </main>
      </Container>
    </div>
  );
}
