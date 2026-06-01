// ─── TemplateCard — gallery card on /templates ──────────────────
//
// Crystal skin over the DS Card. A whole-card-clickable tile: the
// react-router <Link> is the render-as target via <Card asChild>, so
// the consumer (the link) owns activation and the Card owns the look.
//
// Anatomy: a tiny abstract preview at the top (Card.Media), a № +
// category meta Badge, the template name (Card.Title), an italic-feel
// tagline (Card.Description), a three-bullet "what's included" list
// (Card.Content), and a dashed-feel footer band (Card.Footer,
// divider="top") carrying the estimate + a "Use →" affordance.

import { Link } from "react-router-dom";
import { Badge, Card, Stack } from "@zeroship/ui";
import type { Template } from "../lib/templates";
import { estLabel } from "../lib/templates";
import "./TemplateCard.css";

export interface TemplateCardProps {
  template: Template;
}

export function TemplateCard({ template: t }: TemplateCardProps) {
  return (
    <Card asChild interactive variant="outline">
      <Link
        to={`/new?template=${t.slug}`}
        data-testid={`template-card:${t.slug}`}
        className="zs-template-card"
      >
        <Card.Media side="top">
          <Preview num={parseInt(t.num, 10)} />
        </Card.Media>

        <Card.Header>
          <Badge intent="info" variant="soft" size="sm">
            <span className="zs-template-card__meta">
              № {t.num} · {t.category}
            </span>
          </Badge>
          <Card.Title>{t.name}</Card.Title>
          <Card.Description className="zs-template-card__tagline">
            {t.tagline}
          </Card.Description>
        </Card.Header>

        <Card.Content>
          <Stack asChild gap="half">
            <ul className="zs-template-card__bullets">
              {t.bullets.map((b) => (
                <li key={b} className="zs-template-card__bullet">
                  {b}
                </li>
              ))}
            </ul>
          </Stack>
        </Card.Content>

        <Card.Footer align="between" divider="top">
          <span className="zs-template-card__est">{estLabel(t.estSeconds)}</span>
          <span className="zs-template-card__use">Use →</span>
        </Card.Footer>
      </Link>
    </Card>
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
    <div className="zs-template-card__preview" aria-hidden="true">
      {lines.map((row, i) => {
        const multi = row.length > 1;
        return (
          <div
            key={i}
            className="zs-template-card__preview-row"
            data-multi={multi ? "true" : "false"}
          >
            {row.map((kind, j) => (
              <span
                key={j}
                className="zs-template-card__preview-bar"
                data-kind={kind === "cap" ? "cap" : "fill"}
                data-w={multi ? undefined : kind}
              />
            ))}
          </div>
        );
      })}
    </div>
  );
}
