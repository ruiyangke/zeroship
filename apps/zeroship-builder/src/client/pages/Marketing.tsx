// ─── Marketing — public landing page (`/`) ──────────────────────
//
// A bespoke, hand-composed warm-tactile front door — "Sun-Warmed Riso on
// a fine small-press chase." The page leaves the stock DS section bands
// behind and hand-builds semantic <section>/<h2>/<p> markup so the warm
// palette, the offset-print grain, the typographer's rules-and-frames,
// and the "ink doesn't bounce" reveal can be expressed with intent. The
// shared chrome that MUST stay stable — PublicNav, TemplateCard, the
// router Link/useNavigate, the lucide glyphs via the DS <Icon> — is kept
// exactly as before; only the editorial presentation around them is new.
//
// All raw warm colour lives in scoped --mk-* custom properties declared
// ON .zs-marketing in the co-located Marketing.css (never global, no
// Tailwind); the crystal --zs-accent / --zs-surface-bg / --zs-label are
// deliberately remapped inside that scope so the nav button + every
// filled Button inherit terracotta with zero markup forks.
//
// Two headline promises stay prominent: safe from day one, and a
// self-evolving loop that keeps improving the app after it ships. No
// money mechanics anywhere; the voice is calm and human.
//
// PUBLIC — no AuthGuard. The "Begin" CTA shoots the visitor at /new
// (the wizard) which itself is public; createApp is the gate that
// triggers a 401 → /login redirect when the visitor isn't signed in.

import { useEffect, useRef, type CSSProperties } from "react";
import { Link, useNavigate } from "react-router-dom";
import { Container, Grid, Icon } from "@zeroship/ui";
import { Package, RefreshCw, ShieldCheck, Zap } from "lucide-react";
// Load Fraunces with ALL axes (opsz + SOFT + WONK) so the display
// headings can be set soft + high optical-size via font-variation-settings.
// The bare package import resolves to the opsz-only file, which renders the
// SOFT axis dead — the `full` (+ italic) subset entries carry every axis.
import "@fontsource-variable/fraunces/full.css";
import "@fontsource-variable/fraunces/full-italic.css";
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

/** Allow CSS custom properties (--i band/child stagger index) on style. */
type IndexStyle = CSSProperties & { "--i"?: number };

export function Marketing() {
  const navigate = useNavigate();
  const featured = FEATURED_TEMPLATE_SLUGS
    .map((slug) => TEMPLATES.find((t) => t.slug === slug))
    .filter((t): t is (typeof TEMPLATES)[number] => Boolean(t));

  // Below-the-fold bands fade up as they enter the viewport. The static
  // end-state (.is-in / no class) is fully present, so reduced-motion and
  // a never-firing observer both leave a complete, readable page.
  const revealRoot = useRef<HTMLElement>(null);
  useEffect(() => {
    const root = revealRoot.current;
    if (!root) return;
    const targets = Array.from(
      root.querySelectorAll<HTMLElement>("[data-reveal]"),
    );
    // Hero + nav animate on mount; mark them in immediately.
    const onMount = targets.filter((el) => el.dataset.reveal === "mount");
    onMount.forEach((el) => el.classList.add("is-in"));

    const deferred = targets.filter((el) => el.dataset.reveal === "scroll");
    if (deferred.length === 0) return;

    const reduce =
      typeof window !== "undefined" &&
      window.matchMedia?.("(prefers-reduced-motion: reduce)").matches;
    if (reduce || typeof IntersectionObserver === "undefined") {
      deferred.forEach((el) => el.classList.add("is-in"));
      return;
    }

    const io = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          if (entry.isIntersecting) {
            entry.target.classList.add("is-in");
            io.unobserve(entry.target);
          }
        }
      },
      { rootMargin: "0px 0px -12% 0px", threshold: 0.12 },
    );
    deferred.forEach((el) => io.observe(el));
    return () => io.disconnect();
  }, []);

  return (
    <div className="zs-marketing" data-testid="marketing-page">
      <PublicNav />

      <main className="zs-marketing__main" ref={revealRoot}>
        {/* ─── Hero — the one <h1> masthead ─────────────────────── */}
        <section
          className="zs-mk-hero"
          data-reveal="mount"
          aria-labelledby="mk-hero-title"
        >
          <Container size="md" padX={6}>
            <div className="zs-mk-hero__col">
              <p
                className="zs-mk-eyebrow zs-mk-eyebrow--rule"
                style={{ "--i": 0 } as IndexStyle}
              >
                Vol. I · Issue 05 · 2026
              </p>
              <h1
                id="mk-hero-title"
                className="zs-mk-hero__title"
                style={{ "--i": 1 } as IndexStyle}
              >
                Anyone can ship software.
              </h1>
              <p
                className="zs-mk-hero__lede"
                style={{ "--i": 2 } as IndexStyle}
              >
                Describe the thing you'd like to make in one plain sentence —
                a recipe journal, a tip jar, a booking page, anything at all —
                and a quiet fleet of agents writes it, ships it to a real,
                living URL, and keeps making it better. Sign-in, privacy, and
                the careful safety work are{" "}
                <strong>woven in from the very first line</strong>, so your
                data and the people who trust you are looked after without you
                ever having to think about it.
              </p>
              <div
                className="zs-mk-hero__actions"
                style={{ "--i": 3 } as IndexStyle}
              >
                <button
                  type="button"
                  className="zs-mk-btn zs-mk-btn--stamp"
                  onClick={() => navigate("/new")}
                  data-testid="marketing-begin"
                >
                  Begin your project
                </button>
                <Link to="/templates" className="zs-mk-link zs-mk-link--lg">
                  See an example <span aria-hidden="true">→</span>
                </Link>
              </div>
            </div>
          </Container>
        </section>

        {/* ─── How it works — a typographer's table of contents ─── */}
        <section
          className="zs-mk-band zs-mk-band--kraft zs-mk-steps"
          data-reveal="scroll"
          aria-labelledby="mk-steps-title"
          style={{ "--i": 0 } as IndexStyle}
        >
          <Container size="md" padX={6}>
            <header className="zs-mk-head">
              <p className="zs-mk-eyebrow">How it works</p>
              <h2 id="mk-steps-title" className="zs-mk-head__title">
                From a sentence to a living app.
              </h2>
              <p className="zs-mk-head__sub">
                Usually live in under a minute — and it keeps growing from
                there.
              </p>
            </header>

            <ol className="zs-mk-toc">
              {STEPS.map((s, i) => (
                <li
                  key={s.title}
                  className="zs-mk-toc__row"
                  style={{ "--i": i } as IndexStyle}
                >
                  <span className="zs-mk-toc__folio" aria-hidden="true">
                    № {String(i + 1).padStart(2, "0")}
                  </span>
                  <div className="zs-mk-toc__text">
                    <h3 className="zs-mk-toc__title">{s.title}</h3>
                    <p className="zs-mk-toc__body">{s.body}</p>
                  </div>
                </li>
              ))}
            </ol>
          </Container>
        </section>

        {/* ─── Featured templates — engraved calling-card plates ── */}
        <section
          className="zs-mk-band zs-marketing__templates"
          data-testid="marketing-templates"
          data-reveal="scroll"
          aria-labelledby="marketing-templates-title"
          style={{ "--i": 0 } as IndexStyle}
        >
          <Container size="lg" padX={6}>
            <header className="zs-mk-head zs-mk-head--row">
              <div className="zs-mk-head__lead">
                <p className="zs-mk-eyebrow">Starting points</p>
                <h2
                  id="marketing-templates-title"
                  className="zs-mk-head__title"
                >
                  Or begin from a template.
                </h2>
              </div>
              <Link to="/templates" className="zs-mk-link">
                See all <span aria-hidden="true">→</span>
              </Link>
            </header>

            <div className="zs-mk-plates">
              <Grid columns={{ sm: 2, lg: 4 }} gap={5}>
                {featured.map((t, i) => (
                  <div
                    key={t.slug}
                    className="zs-mk-plate"
                    style={{ "--i": i } as IndexStyle}
                  >
                    <TemplateCard template={t} />
                  </div>
                ))}
              </Grid>
            </div>
          </Container>
        </section>

        {/* ─── Why it's different — the four warm-badge promises ── */}
        <section
          className="zs-mk-band zs-mk-diff"
          data-reveal="scroll"
          aria-labelledby="mk-diff-title"
          style={{ "--i": 0 } as IndexStyle}
        >
          <Container size="lg" padX={6}>
            <header className="zs-mk-head">
              <p className="zs-mk-eyebrow">Why it's different</p>
              <h2 id="mk-diff-title" className="zs-mk-head__title">
                Built to be trusted with your livelihood.
              </h2>
              <p className="zs-mk-head__sub">
                Your app keeps improving long after it ships, and it stays
                safe and private the whole way through.
              </p>
            </header>

            <Grid columns={{ sm: 2, lg: 4 }} gap={7}>
              {DIFFERENTIATORS.map((d, i) => (
                <article
                  key={d.title}
                  className="zs-mk-feature"
                  style={{ "--i": i } as IndexStyle}
                >
                  <span className="zs-mk-feature__badge" aria-hidden="true">
                    <Icon as={d.icon} size="lg" />
                  </span>
                  <h3 className="zs-mk-feature__title">{d.title}</h3>
                  <p className="zs-mk-feature__body">{d.body}</p>
                </article>
              ))}
            </Grid>
          </Container>
        </section>

        {/* ─── Proof bar — safe / fast / yours, framed in scotch rules ─ */}
        <section
          className="zs-mk-band zs-mk-band--kraft zs-mk-stats"
          data-reveal="scroll"
          aria-labelledby="mk-stats-title"
          style={{ "--i": 0 } as IndexStyle}
        >
          <Container size="md" padX={6}>
            <header className="zs-mk-head">
              <p className="zs-mk-eyebrow">In short</p>
              <h2 id="mk-stats-title" className="zs-mk-head__title">
                Safe, fast, and yours.
              </h2>
            </header>

            <div className="zs-mk-scotch">
              <dl className="zs-mk-stats__row">
                {STATS.map((stat, i) => (
                  <div
                    key={stat.id}
                    className="zs-mk-stat"
                    style={{ "--i": i } as IndexStyle}
                  >
                    <dt className="zs-mk-stat__value">{stat.value}</dt>
                    <dd className="zs-mk-stat__label">{stat.label}</dd>
                  </div>
                ))}
              </dl>
            </div>
          </Container>
        </section>

        {/* ─── Closing CTA — a printed terracotta seal ──────────── */}
        <section
          className="zs-mk-band zs-mk-cta"
          data-reveal="scroll"
          aria-labelledby="mk-cta-title"
          style={{ "--i": 0 } as IndexStyle}
        >
          <Container size="md" padX={6}>
            <div className="zs-mk-cta__cartouche">
              <p className="zs-mk-eyebrow zs-mk-eyebrow--accent">Begin</p>
              <h2 id="mk-cta-title" className="zs-mk-cta__title">
                Tell us what you'd like to make.
              </h2>
              <p className="zs-mk-cta__desc">
                Start with a single sentence and watch the agents turn it into
                something real, safe, and live — usually in under a minute.
                And they don't stop the moment it ships: change anything,
                anytime, just by asking.
              </p>
              <div className="zs-mk-cta__actions">
                <button
                  type="button"
                  className="zs-mk-btn zs-mk-btn--stamp zs-mk-btn--on-accent"
                  onClick={() => navigate("/new")}
                >
                  Begin your project
                </button>
                <Link
                  to="/templates"
                  className="zs-mk-link zs-mk-link--lg zs-mk-link--on-accent"
                >
                  Browse templates <span aria-hidden="true">→</span>
                </Link>
              </div>
            </div>
          </Container>
        </section>

        {/* ─── Footer — deepest kraft, italic wordmark ──────────── */}
        <footer
          className="zs-mk-band zs-mk-footer"
          data-reveal="scroll"
          aria-label="Site"
        >
          <Container size="lg" padX={6}>
            <div className="zs-mk-footer__top">
              <div className="zs-mk-footer__brand-block">
                <Link to="/" className="zs-marketing__footer-brand">
                  zeroship
                  <span className="zs-marketing__footer-dot">.</span>
                </Link>
                <p className="zs-mk-footer__blurb">Anyone can ship software.</p>
              </div>

              <nav className="zs-mk-footer__cols" aria-label="Footer">
                {FOOTER_COLUMNS.map((col) => (
                  <div key={col.id} className="zs-mk-footer__col">
                    <h2 className="zs-mk-footer__col-title">{col.title}</h2>
                    <ul className="zs-mk-footer__col-list">
                      {col.links.map((l) => (
                        <li key={l.href}>
                          <Link to={l.href} className="zs-mk-link zs-mk-link--footer">
                            {l.label}
                          </Link>
                        </li>
                      ))}
                    </ul>
                  </div>
                ))}
              </nav>
            </div>

            <div className="zs-mk-footer__bottom">
              <p className="zs-mk-footer__copy">&copy; 2026 zeroship</p>
            </div>
          </Container>
        </footer>
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
      "Your app isn't frozen the day it ships. The agents stay on the job — watching how it runs, catching the small things before you'd ever notice, and smoothing the rough edges — and when you want something changed, you just say so.",
  },
  {
    title: "Live in under a minute.",
    icon: Zap,
    body:
      "From a single sentence to a working link you can open and share, usually before your coffee cools. No accounts to wire up, no servers to rent, nothing to configure — you describe it, it appears.",
  },
  {
    title: "Your code is yours.",
    icon: Package,
    body:
      "Everything the agents write belongs to you, and you can take it with you whenever you like. Database, sign-in, payments, and scaling are all included — we host it and keep it running, but you're never locked in.",
  },
];

const STATS: { id: string; value: string; label: string }[] = [
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
    value: "Yours",
    label: "Every line, yours to keep and export",
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
