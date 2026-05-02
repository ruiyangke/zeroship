// ─── Pricing — public pricing surface (`/pricing`) ──────────────
//
// Per spec §5.2. Three-tier card grid (Free / Maker / Pro), each
// with title, big serif price, italic tagline, bullet list, and a
// primary "Choose" button that routes to /signup?plan=<tier>.
//
// Top of page: the "Earn, keep most of it" lede explaining the
// 15 % platform share + Stripe fees, with a worked example mirroring
// the AGENTS.md revenue model.

import { Link } from "react-router-dom";
import { PublicNav } from "../components/PublicNav";
import { StampButton } from "../components/StampButton";
import { GhostButton } from "../components/GhostButton";
import { useNavigate } from "react-router-dom";

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
    <div className="min-h-screen bg-paper" data-testid="pricing-page">
      <PublicNav />

      <main className="mx-auto px-6 pt-14 pb-24" style={{ maxWidth: 960 }}>
        {/* ─── Lede ─────────────────────────────────────────────── */}
        <section className="reveal mb-14 max-w-[700px]">
          <div className="label-uc mb-4 flex items-center gap-2">
            <span className="inline-block h-px w-3.5 bg-ink" aria-hidden="true" />
            Pricing
          </div>
          <h1
            className="font-serif font-medium leading-[0.98] -tracking-[0.02em] mb-5"
            style={{ fontSize: "clamp(48px, 7vw, 80px)" }}
          >
            Earn, keep <em className="italic text-tomato">most</em> of it.
          </h1>
          <p className="font-serif text-[19px] leading-[1.5] text-ink-soft mb-3">
            Subscriptions to zeroship are simple — but the real arithmetic is
            that we earn a 15&nbsp;% share of what your apps make. Nothing
            until you do.
          </p>
          <p className="font-serif italic text-[15px] text-pencil">
            A worked example: a creator's app earns $100/mo. Stripe takes
            ≈$3.20. zeroship takes $15.00. The creator keeps <strong className="not-italic text-ink">$81.80</strong>.
          </p>
        </section>

        {/* ─── Plan cards ───────────────────────────────────────── */}
        <section className="reveal mb-16">
          <div
            className="grid gap-6"
            style={{ gridTemplateColumns: "repeat(auto-fit, minmax(260px, 1fr))" }}
            data-testid="pricing-plans"
          >
            {PLANS.map((p) => (
              <article
                key={p.key}
                data-testid={`pricing-plan-${p.key}`}
                className={
                  p.emphasised
                    ? "bg-white border-2 border-tomato px-7 pt-7 pb-6 relative shadow-[0_24px_28px_-20px_rgba(34,22,12,0.18)]"
                    : "bg-white border border-rule px-7 pt-7 pb-6 relative"
                }
              >
                {p.emphasised && (
                  <div
                    className="absolute -top-3 left-7 bg-tomato text-paper font-sans text-[9.5px] uppercase tracking-[0.2em] px-2 py-0.5"
                    style={{ transform: "rotate(-1deg)" }}
                  >
                    Most chosen
                  </div>
                )}
                <h3 className="font-serif italic text-[15px] text-tomato mb-2">{p.name}</h3>
                <div className="flex items-baseline gap-2 mb-1.5">
                  <span
                    className="font-serif font-medium text-[44px] -tracking-[0.02em] leading-none"
                    style={{ fontVariationSettings: '"opsz" 144' }}
                  >
                    {p.price}
                  </span>
                  <span className="font-serif italic text-[14px] text-ink-soft">{p.cadence}</span>
                </div>
                <p className="font-serif italic text-[14px] text-ink-soft mb-5 leading-snug">
                  {p.tagline}
                </p>
                <ul className="m-0 p-0 list-none mb-6 font-serif text-[14.5px] text-ink leading-[1.6]">
                  {p.features.map((f) => (
                    <li
                      key={f}
                      className="py-0.5 before:content-['·'] before:text-tomato before:font-bold before:mr-2"
                    >
                      {f}
                    </li>
                  ))}
                </ul>
                {p.emphasised ? (
                  <StampButton
                    onClick={() => choose(p.key)}
                    data-testid={`pricing-cta-${p.key}`}
                  >
                    {p.cta}
                  </StampButton>
                ) : (
                  <GhostButton
                    onClick={() => choose(p.key)}
                    data-testid={`pricing-cta-${p.key}`}
                  >
                    {p.cta} →
                  </GhostButton>
                )}
              </article>
            ))}
          </div>
        </section>

        {/* ─── 15 % share band ──────────────────────────────────── */}
        <section className="reveal mb-14 bg-paper-2 border border-rule px-9 py-8 max-w-[760px]">
          <div className="label-uc mb-3 flex items-center gap-2">
            <span className="inline-block h-px w-3.5 bg-ink" aria-hidden="true" />
            The 15&nbsp;% share, plainly
          </div>
          <h2 className="font-serif font-medium text-[28px] -tracking-[0.015em] mb-3 leading-tight">
            Aligned incentives, no platform fee until you sell.
          </h2>
          <p className="font-serif text-[16px] leading-[1.6] text-ink-soft m-0">
            zeroship makes money <em className="italic">only</em> when your
            apps make money. Run a free side project — the platform takes
            nothing. Charge for it — Stripe takes its standard fees and we
            take 15&nbsp;% of what's left. Infrastructure, agents, hosting,
            scaling, monitoring: all included.
          </p>
        </section>

        {/* ─── Enterprise note ──────────────────────────────────── */}
        <section className="reveal mb-12 max-w-[700px]">
          <p className="font-serif italic text-[15px] text-ink-soft m-0">
            Need an audit log, SSO, a custom contract, or volume pricing?{" "}
            <Link
              to="/about"
              className="text-tomato hover:opacity-80"
              style={{ textDecoration: "underline", textDecorationStyle: "dotted" }}
            >
              Get in touch
            </Link>{" "}
            — we'll work something out.
          </p>
        </section>

        <hr className="hairline mb-10" />

        <footer className="font-sans text-[11px] uppercase tracking-[0.2em] text-pencil flex flex-wrap gap-x-7 gap-y-3 items-center">
          <Link to="/" className="hover:text-ink transition-colors" style={{ textDecoration: "none", color: "inherit" }}>Home</Link>
          <Link to="/templates" className="hover:text-ink transition-colors" style={{ textDecoration: "none", color: "inherit" }}>Templates</Link>
          <Link to="/skills" className="hover:text-ink transition-colors" style={{ textDecoration: "none", color: "inherit" }}>Skills</Link>
          <Link to="/about" className="hover:text-ink transition-colors" style={{ textDecoration: "none", color: "inherit" }}>About</Link>
          <span className="ml-auto font-serif italic text-[12px] tracking-normal normal-case text-pencil">
            zeroship<span className="text-tomato">.</span> &copy; 2026
          </span>
        </footer>
      </main>
    </div>
  );
}
