// ─── Templates — public gallery (`/templates`) ──────────────────
//
// Per spec §5.4. Filter chips by category + grid of TemplateCards.
// Public route — uses PublicNav (not TopBar/PageFrame which expect
// an authed user). Click any card → /new?template=<slug>; the
// wizard reads the param and pre-fills the brief.

import { useState } from "react";
import { Link } from "react-router-dom";
import { PublicNav } from "../components/PublicNav";
import { FilterPill } from "../components/FilterPill";
import { TemplateCard } from "../components/TemplateCard";
import { TEMPLATES, TEMPLATE_CATEGORIES, type TemplateCategory } from "../lib/templates";

export function Templates() {
  const [filter, setFilter] = useState<"all" | TemplateCategory>("all");
  const visible = filter === "all" ? TEMPLATES : TEMPLATES.filter((t) => t.category === filter);

  return (
    <div className="min-h-screen bg-paper" data-testid="templates-page">
      <PublicNav />

      <main className="mx-auto px-6 pt-14 pb-24" style={{ maxWidth: 960 }}>
        <section className="reveal mb-8 max-w-[680px]">
          <div className="label-uc mb-4 flex items-center gap-2">
            <span className="inline-block h-px w-3.5 bg-ink" aria-hidden="true" />
            Templates
          </div>
          <h1
            className="font-serif font-medium leading-[0.98] -tracking-[0.02em] mb-3"
            style={{ fontSize: "clamp(48px, 7vw, 80px)" }}
          >
            Pick a <em className="italic text-tomato">starting point</em>.
          </h1>
          <p className="font-serif text-[18px] leading-[1.55] text-ink-soft m-0">
            A dozen templates organised by what you're trying to do — share,
            collect, sell, show. Or describe your own.
          </p>
        </section>

        <div className="flex flex-wrap gap-2.5 mb-8" data-testid="templates-filters">
          {TEMPLATE_CATEGORIES.map(({ key, label }) => (
            <FilterPill
              key={key}
              active={filter === key}
              onClick={() => setFilter(key as any)}
              data-testid={`filter:${key}`}
            >
              {label}
            </FilterPill>
          ))}
        </div>

        {visible.length === 0 ? (
          <div
            data-testid="templates-empty"
            className="border border-dashed border-rule p-10 text-center"
          >
            <p className="font-serif italic text-ink-soft text-[15px] mb-1">
              Nothing in <em>{filter}</em> yet — that shelf is still being stocked.
            </p>
            <button
              type="button"
              onClick={() => setFilter("all")}
              className="font-serif italic text-[14px] text-tomato bg-transparent border-0 cursor-pointer hover:opacity-80 mt-2 focus:outline-2 focus:outline-tomato focus:outline-offset-2"
            >
              ← Show all templates
            </button>
          </div>
        ) : (
          <div
            className="grid gap-5"
            style={{ gridTemplateColumns: "repeat(auto-fill, minmax(260px, 1fr))" }}
            data-testid="templates-grid"
          >
            {visible.map((t) => <TemplateCard key={t.slug} template={t} />)}
          </div>
        )}

        <div className="mt-12 mb-14">
          <Link
            to="/new"
            className="inline-block font-serif italic text-[16px] text-ink border-b border-rule hover:text-tomato hover:border-tomato py-3"
            style={{ textDecoration: "none" }}
            data-testid="templates-blank"
          >
            Or describe your own →
          </Link>
        </div>

        <hr className="hairline mb-10" />

        <footer className="font-sans text-[11px] uppercase tracking-[0.2em] text-pencil flex flex-wrap gap-x-7 gap-y-3 items-center">
          <Link to="/" className="hover:text-ink transition-colors" style={{ textDecoration: "none", color: "inherit" }}>Home</Link>
          <Link to="/pricing" className="hover:text-ink transition-colors" style={{ textDecoration: "none", color: "inherit" }}>Pricing</Link>
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
