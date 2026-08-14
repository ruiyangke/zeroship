import { Badge } from "@zeroship/ui";

import type { IssueKind, IssuePriority, IssueSeverity } from "../lib/quicksearch";

/**
 * Status, resolution, kind, severity and priority as design-system badges.
 *
 * These were bespoke spans with hand-picked hex colours, which is how the app
 * ended up with a green primary sitting next to the system's blue accent. The
 * intent scale carries the meaning now, so a theme change moves them too.
 *
 * Intent is assigned by MEANING, not by rank. `critical` and `blocker` are
 * danger because someone must act. Mapping severity onto a red-to-green ramp
 * would make `trivial` look like a success.
 */

const STATUS_INTENT: Record<string, "neutral" | "info" | "success" | "warning" | "danger"> = {
  UNCONFIRMED: "neutral",
  CONFIRMED: "info",
  IN_PROGRESS: "info",
  RESOLVED: "success",
  VERIFIED: "success",
  CLOSED: "neutral",
};

const SEVERITY_INTENT: Record<string, "neutral" | "info" | "success" | "warning" | "danger"> = {
  blocker: "danger",
  critical: "danger",
  major: "warning",
  normal: "neutral",
  minor: "neutral",
  trivial: "neutral",
};

// Kind answers "what is this", so none of the three is better or worse than
// another and none is danger: a critical enhancement is spelled by pairing
// `enhancement` with `critical`, which is the whole reason the value left the
// severity list.
const KIND_INTENT: Record<string, "neutral" | "info" | "success" | "warning" | "danger"> = {
  defect: "neutral",
  enhancement: "info",
  task: "neutral",
};

// P1/P2 read as urgent, P3 as the default, P4/P5 as explicitly deprioritised.
const PRIORITY_INTENT: Record<string, "neutral" | "info" | "success" | "warning" | "danger"> = {
  P1: "danger",
  P2: "warning",
  P3: "neutral",
  P4: "neutral",
  P5: "neutral",
};

export function StatusBadge({ status }: { status: string }) {
  return (
    <Badge intent={STATUS_INTENT[status] ?? "neutral"} variant="soft" size="sm">
      {status.replace("_", " ")}
    </Badge>
  );
}

export function ResolutionBadge({ resolution }: { resolution: string | null }) {
  // An unresolved issue renders nothing rather than a "--" placeholder. A column
  // of dashes is noise that reads as data.
  if (!resolution) return null;
  return (
    <Badge intent={resolution === "FIXED" ? "success" : "neutral"} variant="outline" size="sm">
      {resolution}
    </Badge>
  );
}

export function KindBadge({ kind }: { kind: IssueKind | string }) {
  return (
    <Badge intent={KIND_INTENT[kind] ?? "neutral"} variant="soft" size="sm">
      {kind}
    </Badge>
  );
}

export function SeverityBadge({ severity }: { severity: IssueSeverity | string }) {
  return (
    <Badge intent={SEVERITY_INTENT[severity] ?? "neutral"} variant="soft" size="sm">
      {severity}
    </Badge>
  );
}

export function PriorityBadge({ priority }: { priority: IssuePriority | string }) {
  return (
    <Badge intent={PRIORITY_INTENT[priority] ?? "neutral"} variant="outline" size="sm">
      {priority}
    </Badge>
  );
}
