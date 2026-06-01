// ─── ProjectCard — crystal ──────────────────────────────────────
//
// One project tile on the home gallery. Built over the @zeroship/ui
// Card: a whole-card-clickable surface (Card asChild → react-router
// <Link>) carrying an index eyebrow, the project title, a tagline
// description, and a footer band with status + last-update meta.

import { Link } from "react-router-dom";
import { Badge, Card } from "@zeroship/ui";
import type { ProjectRecord } from "../api";
import "./ProjectCard.css";

export interface ProjectCardProps {
  app: ProjectRecord;
  num: string;
  /** Tagline override — defaults to the project's name styled-italically. */
  tagline?: string;
}

export function ProjectCard({ app, num, tagline }: ProjectCardProps) {
  const updated = fmtDate(app.updated_at);
  const display = tagline ?? `An app called ${app.name}.`;

  return (
    <Card variant="surface" interactive asChild className="pcard">
      <Link
        to={`/p/${app.id}/preview`}
        data-testid={`project-card:${app.name}`}
      >
        <Card.Header>
          <span className="pcard__num" aria-hidden="true">
            № {num}
          </span>
          <Card.Title className="pcard__title">{app.name}</Card.Title>
          <Card.Description className="pcard__tagline">
            {display}
          </Card.Description>
        </Card.Header>
        <Card.Footer align="between" divider="top" className="pcard__footer">
          <Badge
            intent={app.archived ? "neutral" : "info"}
            variant="soft"
            size="sm"
          >
            {app.archived ? "Archived" : "Draft"}
          </Badge>
          <span className="pcard__updated">upd. {updated}</span>
        </Card.Footer>
      </Link>
    </Card>
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
