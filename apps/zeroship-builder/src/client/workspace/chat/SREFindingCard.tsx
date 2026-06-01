// SRE finding card. Builder dispatches SRE via task("sre", …) when the
// user asks reliability questions ("why is the app slow?"). Middleware
// extracts the structured response (see internal/middleware.ts ->
// normaliseSREFinding) and emits `data-sre-finding` which ChatMessages
// dispatches here.
//
// Crystal migration: the card frames on the DS Card (outline), the
// severity word rides a DS Badge, and the related-logs disclosure is a
// DS Collapsible. Severity also drives a bespoke left-edge tone bar via
// --zs-system-* tokens (see SREFindingCard.css). The public interface
// (the `finding` prop, the `SREFindingCard` export), the diagnosis /
// recommendation / logs rendering, the expand-collapse behaviour, and
// the data-testid hooks (`sre-finding-card`, `sre-logs-toggle`) are
// preserved exactly.
//
// Visual contract:
//  - Severity drives the left-edge tone bar AND the Badge intent:
//      info     → neutral (just FYI)
//      warning  → warning (degraded but live)
//      error    → danger  (broken for some users)
//      critical → danger, solid (broken for all / data loss)
//  - Diagnosis = headline / first thing the user reads.
//  - Recommendation = the actionable bit.
//  - related_logs = optional, collapsed by default to keep the card
//    compact when SRE doesn't ground in logs.

import { Badge, Card, Collapsible } from "@zeroship/ui";
import type { BadgeIntent, BadgeVariant } from "@zeroship/ui";
import type { SREFinding } from "../../types/chat";
import "./SREFindingCard.css";

const SEVERITY_BADGE: Record<
  string,
  { label: string; intent: BadgeIntent; variant: BadgeVariant }
> = {
  info: { label: "info", intent: "neutral", variant: "soft" },
  warning: { label: "warning", intent: "warning", variant: "soft" },
  error: { label: "error", intent: "danger", variant: "soft" },
  critical: { label: "critical", intent: "danger", variant: "solid" },
};

export function SREFindingCard({ finding }: { finding: SREFinding }) {
  const severity = SEVERITY_BADGE[finding.severity] ?? SEVERITY_BADGE.info!;
  const logs = finding.related_logs ?? [];

  return (
    <Card
      variant="outline"
      data-testid="sre-finding-card"
      data-severity={finding.severity}
      className="sre-finding"
    >
      <span className="sre-finding__bar" aria-hidden />

      <div className="sre-finding__header">
        <span className="sre-finding__eyebrow">sre · finding</span>
        <Badge intent={severity.intent} variant={severity.variant} size="sm">
          {severity.label}
        </Badge>
      </div>

      <div className="sre-finding__body">
        <div className="sre-finding__diagnosis">{finding.diagnosis}</div>
        {finding.recommendation && (
          <div className="sre-finding__recommendation">
            <span className="sre-finding__fix-eyebrow">fix</span>
            {finding.recommendation}
          </div>
        )}
      </div>

      {logs.length > 0 && (
        <Collapsible className="sre-finding__logs" defaultOpen={false}>
          <Collapsible.Trigger
            data-testid="sre-logs-toggle"
            className="sre-finding__logs-trigger"
          >
            related logs ({logs.length})
          </Collapsible.Trigger>
          <Collapsible.Panel>
            <div className="sre-finding__logs-list">
              {logs.map((l, i) => (
                <div key={i}>
                  <div className="sre-finding__log-source">{l.source}</div>
                  <pre className="sre-finding__log-excerpt">{l.excerpt}</pre>
                </div>
              ))}
            </div>
          </Collapsible.Panel>
        </Collapsible>
      )}
    </Card>
  );
}
