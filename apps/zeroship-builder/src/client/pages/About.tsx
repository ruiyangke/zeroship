// ─── About — public manifesto (`/about`) ────────────────────────
//
// Per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §5.5. Editorial single-column page. One or two paragraphs
// of voice — the philosophy, the why, the bet. Not a press kit.

import { Link } from "react-router-dom";
import { PublicNav } from "../components/PublicNav";

export function About() {
  return (
    <div className="min-h-screen bg-paper" data-testid="about-page">
      <PublicNav />

      <main className="mx-auto px-6 pt-14 pb-24" style={{ maxWidth: 700 }}>
        <section className="reveal mb-12">
          <div className="label-uc mb-4 flex items-center gap-2">
            <span className="inline-block h-px w-3.5 bg-ink" aria-hidden="true" />
            Manifesto
          </div>
          <h1
            className="font-serif font-medium leading-[0.98] -tracking-[0.02em] mb-7"
            style={{ fontSize: "clamp(46px, 6vw, 72px)" }}
          >
            Software, made by{" "}
            <em className="italic text-tomato">anyone</em>.
          </h1>
        </section>

        <article className="reveal font-serif text-[19px] leading-[1.6] text-ink space-y-7 mb-14">
          <p>
            For thirty years, shipping software has been a craft fenced off by
            a paywall of patience: months of learning, frameworks that change
            yearly, hosting bills that compound, the slow grind of becoming
            fluent enough to be dangerous. The people who needed software the
            most — small operators, side-project dreamers, hobbyists with one
            very good idea — almost always couldn't get there.
          </p>

          <p>
            <em className="italic">zeroship</em> is the bet that this changes
            now. Describe what you want in a sentence. A fleet of agents — a
            Builder, a Critic, a Reviewer, a PM, an SRE — writes the code,
            reviews the code, deploys it, monitors it, and helps you charge
            for it when you're ready. The platform takes 15&nbsp;%. You keep
            the rest. You also keep the code: it exports as a{" "}
            <code className="font-mono text-[15px] not-italic">.zship</code>{" "}
            bundle, anywhere, anytime.
          </p>

          <p>
            The aesthetic is editorial because tools deserve more than the
            chrome of dashboards. The infrastructure is bespoke — a V8
            runtime on io_uring, no tokio in the stack, native primitives
            small and stable on purpose — because economics matter. A
            creator's app costs us about twelve cents a month to host. Their
            app should pay them, not us.
          </p>

          <p>
            We're early. Some surfaces are stubs. The skill registry isn't
            wired yet. There's a long list of "coming soon" notes in our
            issue tracker, all of them honest. If you'd like to help — or
            just see what the agents make of your idea —{" "}
            <Link to="/" className="text-tomato hover:opacity-80" style={{ textDecorationStyle: "dotted" }}>
              start a project
            </Link>
            . It usually takes under a minute.
          </p>
        </article>

        <hr className="hairline mb-10" />

        <footer className="font-sans text-[11px] uppercase tracking-[0.2em] text-pencil flex flex-wrap gap-x-7 gap-y-3 items-center">
          <Link to="/" className="hover:text-ink transition-colors" style={{ textDecoration: "none", color: "inherit" }}>Home</Link>
          <Link to="/pricing" className="hover:text-ink transition-colors" style={{ textDecoration: "none", color: "inherit" }}>Pricing</Link>
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
