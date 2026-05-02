// ─── TemplateCard — gallery card on /templates ──────────────────
//
// Mini-preview at the top (placeholder shapes — gives a sense of
// shape without rendering the actual template), № in tomato italic,
// title, italic tagline, three-bullet "what's included" list,
// dashed footer with estimated time + Use → affordance.

import { Link } from "react-router-dom";
import type { Template } from "../lib/templates";
import { estLabel } from "../lib/templates";

export interface TemplateCardProps {
  template: Template;
}

export function TemplateCard({ template: t }: TemplateCardProps) {
  return (
    <Link
      to={`/new?template=${t.slug}`}
      data-testid={`template-card:${t.slug}`}
      className="block bg-white border border-rule px-6 pt-5 pb-4 transition-all duration-300 hover:-translate-y-[3px] hover:shadow-[0_24px_28px_-20px_rgba(34,22,12,0.16)]"
      style={{ textDecoration: "none", color: "var(--color-ink)" }}
    >
      <Preview num={parseInt(t.num, 10)} />
      <div className="font-serif italic text-[11.5px] text-tomato tracking-wide mb-1" style={{ fontFeatureSettings: '"lnum" 1' }}>
        № {t.num} · <span className="capitalize">{t.category}</span>
      </div>
      <h4 className="font-serif font-medium text-[21px] leading-[1.05] mb-1 -tracking-[0.015em]">
        {t.name}
      </h4>
      <p className="font-serif italic text-[13px] text-ink-soft mb-4 leading-snug">
        {t.tagline}
      </p>
      <ul className="m-0 p-0 list-none mb-4 font-serif text-[13px] text-ink-soft">
        {t.bullets.map((b) => (
          <li key={b} className="py-px before:content-['·'] before:text-tomato before:font-bold before:mr-2">
            {b}
          </li>
        ))}
      </ul>
      <div className="flex justify-between items-baseline pt-3 border-t border-dashed border-rule">
        <span className="font-sans text-[10px] uppercase tracking-[0.16em] text-pencil">
          {estLabel(t.estSeconds)}
        </span>
        <span className="font-serif italic text-[13px] text-tomato">Use →</span>
      </div>
    </Link>
  );
}

/** Tiny placeholder preview — varies layout based on the template №
 *  so the gallery feels lively without needing real screenshots. */
function Preview({ num }: { num: number }) {
  // Pick one of 4 shape sets deterministically
  const variants: Array<("short" | "med" | "full" | "cap")[][]> = [
    [["short"], ["med"], ["full", "full", "cap"]],
    [["short"], ["full", "full", "full"], ["full", "cap", "full"]],
    [["short"], ["med"], ["full"], ["short"]],
    [["med"], ["short"], ["cap"]],
  ];
  const lines = variants[num % variants.length];

  return (
    <div className="bg-paper-2 h-[96px] mb-3 px-2 py-2 flex flex-col gap-1 border border-rule-2">
      {lines.map((row, i) => (
        <div key={i} className="flex gap-1 first:flex-col first:gap-1">
          {row.map((kind, j) => (
            <span
              key={j}
              className={
                kind === "cap"   ? "bg-tomato opacity-70" :
                                   "bg-ink opacity-[0.18]"
              }
              style={{
                height: row.length > 1 ? "18px" : "5px",
                width:
                  kind === "short" ? "35%" :
                  kind === "med"   ? "70%" :
                                     "100%",
                flex: row.length > 1 ? 1 : "initial",
              }}
            />
          ))}
        </div>
      ))}
    </div>
  );
}
