// ─── Changelog — public chronology (`/changelog`) ───────────────
//
// Per spec §5.5. A static, editorial list of recent changes. Entries
// are dated and described in the same voice as the app. New entries
// go on top.

import { Link } from "react-router-dom";
import { PublicNav } from "../components/PublicNav";

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
    <div className="min-h-screen bg-paper" data-testid="changelog-page">
      <PublicNav />

      <main className="mx-auto px-6 pt-14 pb-24" style={{ maxWidth: 760 }}>
        <section className="reveal mb-10 max-w-[680px]">
          <div className="label-uc mb-4 flex items-center gap-2">
            <span className="inline-block h-px w-3.5 bg-ink" aria-hidden="true" />
            Changelog
          </div>
          <h1
            className="font-serif font-medium leading-[0.98] -tracking-[0.02em] mb-3"
            style={{ fontSize: "clamp(46px, 6vw, 72px)" }}
          >
            Latest <em className="italic text-tomato">changes</em>.
          </h1>
          <p className="font-serif text-[18px] leading-[1.55] text-ink-soft m-0">
            What's new, what's fixed, what's polished. New entries go on top.
          </p>
        </section>

        <ol className="m-0 p-0 list-none mb-14" data-testid="changelog-entries">
          {ENTRIES.map((e) => (
            <li
              key={e.date + e.title}
              className="grid gap-6 py-7 border-t border-rule"
              style={{ gridTemplateColumns: "120px 1fr" }}
            >
              <div>
                <div className="font-mono text-[12px] text-ink-soft tracking-wide">
                  {e.date}
                </div>
                {e.tag && (
                  <div className="mt-2 font-sans text-[10px] uppercase tracking-[0.2em] text-tomato">
                    {e.tag}
                  </div>
                )}
              </div>
              <div>
                <h3 className="font-serif font-medium text-[22px] -tracking-[0.012em] leading-tight mb-2">
                  {e.title}
                </h3>
                <p className="font-serif text-[16px] leading-[1.55] text-ink-soft m-0">
                  {e.body}
                </p>
              </div>
            </li>
          ))}
        </ol>

        <hr className="hairline mb-10" />

        <footer className="font-sans text-[11px] uppercase tracking-[0.2em] text-pencil flex flex-wrap gap-x-7 gap-y-3 items-center">
          <Link to="/" className="hover:text-ink transition-colors" style={{ textDecoration: "none", color: "inherit" }}>Home</Link>
          <Link to="/pricing" className="hover:text-ink transition-colors" style={{ textDecoration: "none", color: "inherit" }}>Pricing</Link>
          <Link to="/templates" className="hover:text-ink transition-colors" style={{ textDecoration: "none", color: "inherit" }}>Templates</Link>
          <Link to="/about" className="hover:text-ink transition-colors" style={{ textDecoration: "none", color: "inherit" }}>About</Link>
          <span className="ml-auto font-serif italic text-[12px] tracking-normal normal-case text-pencil">
            zeroship<span className="text-tomato">.</span> &copy; 2026
          </span>
        </footer>
      </main>
    </div>
  );
}
