// ─── Templates — gallery of starting points ─────────────────────
//
// Top: filter chips by category. Below: grid of TemplateCards.
// Bottom: "Or describe your own →" exit. Click any card → /new.

import { useState } from "react";
import { Link } from "react-router-dom";
import { PageFrame } from "../components/PageFrame";
import { FilterPill } from "../components/FilterPill";
import { TemplateCard } from "../components/TemplateCard";
import { TEMPLATES, TEMPLATE_CATEGORIES, type TemplateCategory } from "../lib/templates";

export function Templates() {
  const [filter, setFilter] = useState<"all" | TemplateCategory>("all");
  const visible = filter === "all" ? TEMPLATES : TEMPLATES.filter((t) => t.category === filter);

  return (
    <PageFrame
      crumb={[{ label: "studio", to: "/" }, { label: "templates" }]}
      maxWidth={960}
      showMarginalia={false}
    >
      <section className="reveal">
        <h1
          className="font-serif font-medium leading-[0.98] -tracking-[0.02em] mb-3"
          style={{ fontSize: "clamp(40px, 5vw, 64px)" }}
        >
          Pick a <em className="italic text-tomato">starting point</em>.
        </h1>
        <p className="font-serif text-[17px] leading-[1.55] text-ink-soft max-w-[600px] mb-7">
          A dozen templates organised by what you're trying to do — share, collect, sell, show.
          Or describe your own.
        </p>

        <div className="flex flex-wrap gap-2.5 mb-7" data-testid="templates-filters">
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

        <div className="grid gap-5" style={{ gridTemplateColumns: "repeat(auto-fill, minmax(260px, 1fr))" }} data-testid="templates-grid">
          {visible.map((t) => <TemplateCard key={t.slug} template={t} />)}
        </div>

        <div className="mt-10">
          <Link
            to="/new"
            className="inline-block font-serif italic text-[16px] text-ink border-b border-rule hover:text-tomato hover:border-tomato py-3"
            style={{ textDecoration: "none" }}
            data-testid="templates-blank"
          >
            Or describe your own →
          </Link>
        </div>
      </section>
    </PageFrame>
  );
}
