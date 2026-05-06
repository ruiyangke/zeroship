// ─── Skills — public skill catalogue (`/skills`) ────────────────
//
// Per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §5.3. Filter pills at the top, grid of skill cards
// underneath. "Add to project" is greyed out / "Coming soon" — the
// install action and skill registry are not wired yet.

import { useState } from "react";
import { Link } from "react-router-dom";
import { PublicNav } from "../components/PublicNav";
import { FilterPill } from "../components/FilterPill";
import { SKILLS, SKILL_CATEGORIES, type SkillCategory, type Skill } from "../lib/skills";

export function Skills() {
  const [filter, setFilter] = useState<"all" | SkillCategory>("all");
  const visible = filter === "all" ? SKILLS : SKILLS.filter((s) => s.category === filter);

  return (
    <div className="min-h-screen bg-paper" data-testid="skills-page">
      <PublicNav />

      <main className="mx-auto px-6 pt-14 pb-24" style={{ maxWidth: 960 }}>
        {/* ─── Lede ─────────────────────────────────────────────── */}
        <section className="reveal mb-10 max-w-[680px]">
          <div className="label-uc mb-4 flex items-center gap-2">
            <span className="inline-block h-px w-3.5 bg-ink" aria-hidden="true" />
            Skills catalogue
          </div>
          <h1
            className="font-serif font-medium leading-[0.98] -tracking-[0.02em] mb-5"
            style={{ fontSize: "clamp(48px, 7vw, 80px)" }}
          >
            What your app can <em className="italic text-tomato">do</em>.
          </h1>
          <p className="font-serif text-[18px] leading-[1.55] text-ink-soft m-0">
            Skills are the building blocks: auth, payments, search, AI,
            realtime. Add them to a project and the agents wire them in for
            you. No SDKs to read, no API keys to copy.
          </p>
        </section>

        {/* ─── Filter pills ─────────────────────────────────────── */}
        <div className="flex flex-wrap gap-2.5 mb-8" data-testid="skills-filters">
          {SKILL_CATEGORIES.map(({ key, label }) => (
            <FilterPill
              key={key}
              active={filter === key}
              onClick={() => setFilter(key as any)}
              data-testid={`skills-filter:${key}`}
            >
              {label}
            </FilterPill>
          ))}
        </div>

        {/* ─── Skill cards ──────────────────────────────────────── */}
        {visible.length === 0 ? (
          <div
            data-testid="skills-empty"
            className="border border-dashed border-rule p-10 text-center mb-14"
          >
            <p className="font-serif italic text-ink-soft text-[15px] mb-1">
              No skills in <em>{filter}</em> yet — the catalogue's still wiring up.
            </p>
            <button
              type="button"
              onClick={() => setFilter("all")}
              className="font-serif italic text-[14px] text-tomato bg-transparent border-0 cursor-pointer hover:opacity-80 mt-2 focus:outline-2 focus:outline-tomato focus:outline-offset-2"
            >
              ← Show all skills
            </button>
          </div>
        ) : (
          <div
            className="grid gap-5 mb-14"
            style={{ gridTemplateColumns: "repeat(auto-fill, minmax(260px, 1fr))" }}
            data-testid="skills-grid"
          >
            {visible.map((s) => <SkillCard key={s.slug} skill={s} />)}
          </div>
        )}

        {/* ─── Note about the registry ──────────────────────────── */}
        <section className="reveal mb-14 bg-paper-2 border border-rule px-7 py-6 max-w-[680px]">
          <div className="label-uc mb-2">A note</div>
          <p className="font-serif italic text-[15px] text-ink-soft m-0 leading-[1.55]">
            The skill registry is wiring up. For now this page lists what's
            on the roadmap; "Add to project" lights up once the install
            action ships.
          </p>
        </section>

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

function SkillCard({ skill }: { skill: Skill }) {
  return (
    <article
      data-testid={`skill-card:${skill.slug}`}
      className="bg-white border border-rule px-6 pt-5 pb-4 transition-all duration-300 hover:-translate-y-[2px] hover:shadow-[0_24px_28px_-20px_rgba(34,22,12,0.16)]"
    >
      <div
        className="font-serif italic text-[11.5px] text-tomato tracking-wide mb-2"
        style={{ fontFeatureSettings: '"lnum" 1' }}
      >
        № {skill.num} · <span className="capitalize">{skill.category}</span>
      </div>
      <div className="flex items-center gap-3 mb-2">
        <span aria-hidden="true" className="text-[26px] leading-none select-none">
          {skill.icon}
        </span>
        <h3 className="font-serif font-medium text-[22px] -tracking-[0.015em] leading-tight m-0">
          {skill.name}
        </h3>
      </div>
      <p className="font-serif italic text-[14px] text-ink-soft mb-5 leading-snug">
        {skill.tagline}
      </p>
      <div className="flex justify-between items-baseline pt-3 border-t border-dashed border-rule">
        <span className="font-sans text-[10px] uppercase tracking-[0.16em] text-pencil">
          Coming soon
        </span>
        <button
          type="button"
          disabled
          className="font-serif italic text-[13px] text-pencil cursor-not-allowed bg-transparent border-0 p-0"
          data-testid={`skill-add:${skill.slug}`}
        >
          Add to project →
        </button>
      </div>
    </article>
  );
}
