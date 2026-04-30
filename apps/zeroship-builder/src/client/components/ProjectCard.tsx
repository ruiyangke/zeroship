// ─── ProjectCard — letterhead-style ─────────────────────────────
//
// One project on the home gallery. № in tomato italic, big serif
// title, italic tagline (the project's prompt or description), a
// dashed footing rule with status + last-update meta.

import { Link } from "react-router-dom";
import type { AppRecord } from "../api";

export interface ProjectCardProps {
  app: AppRecord;
  num: string;
  /** Tagline override — defaults to the app's name styled-italically. */
  tagline?: string;
}

export function ProjectCard({ app, num, tagline }: ProjectCardProps) {
  const live = !!app.deploy_hash;
  const updated = fmtDate(app.updated_at);
  const display = tagline ?? `An app called ${app.name}.`;

  return (
    <Link
      to={`/p/${app.id}/preview`}
      data-testid={`project-card:${app.name}`}
      className="pcard group relative bg-white border border-rule p-6 pb-4 transition-all duration-300 hover:-translate-y-[3px] hover:shadow-[0_28px_32px_-20px_rgba(34,22,12,0.18),0_6px_12px_-8px_rgba(34,22,12,0.08)]"
      style={{ textDecoration: "none", color: "var(--color-ink)" }}
    >
      {/* paper-fold corner */}
      <span
        className="absolute top-0 right-0 transition-all duration-300 group-hover:w-8 group-hover:h-8"
        style={{
          width: "22px",
          height: "22px",
          background: "linear-gradient(225deg, var(--color-paper) 50%, transparent 50%)",
          borderLeft: "1px solid var(--color-rule)",
          borderBottom: "1px solid var(--color-rule)",
        }}
        aria-hidden="true"
      />
      <div className="font-serif italic text-[12px] text-tomato mb-3 tracking-wide" style={{ fontFeatureSettings: '"lnum" 1' }}>
        № {num}
      </div>
      <h4 className="font-serif font-medium text-[22px] leading-[1.05] mb-2 -tracking-[0.015em] truncate">
        {app.name}
      </h4>
      <p className="font-serif italic text-[14px] text-ink-soft leading-snug mb-5 line-clamp-2">
        {display}
      </p>
      <div className="flex justify-between items-baseline pt-3 border-t border-dashed border-rule font-sans text-[10.5px] uppercase tracking-[0.16em] text-pencil">
        {live ? (
          <span className="text-tomato font-semibold inline-flex items-center gap-1.5">
            <span
              className="inline-block size-[5px] rounded-full bg-tomato pulse-dot"
              style={{ boxShadow: "0 0 0 2.5px rgba(220,75,50,0.18)" }}
              aria-hidden="true"
            />
            Live
          </span>
        ) : (
          <span>Draft</span>
        )}
        <span>upd. {updated}</span>
      </div>
    </Link>
  );
}

function fmtDate(s?: string): string {
  if (!s) return "—";
  try {
    const d = new Date(s);
    if (isNaN(d.getTime())) return s;
    const ms = Date.now() - d.getTime();
    const sec = Math.floor(ms / 1000);
    if (sec < 60)    return "just now";
    if (sec < 3600)  return `${Math.floor(sec / 60)}m ago`;
    if (sec < 86400) return `${Math.floor(sec / 3600)}h ago`;
    if (sec < 604800) return `${Math.floor(sec / 86400)}d ago`;
    return d.toISOString().slice(0, 10);
  } catch { return s; }
}
