"use server";
// Server-internal writes for agent surfaces — kept out of the public
// RPC namespace by the underscore-prefix file convention.
//
// The chat middleware (`_middleware.ts`) calls these after every
// agent SubAgent dispatch to persist round-by-round state into the
// project's KV slot. The READ side of the same data ships from
// `agents.ts` (`getQualityScores`, `listIssues`, etc.) which the
// canvases call as plain RPC procs. Splitting reads (public) from
// writes (server-only) keeps the canvas's public surface read-only-
// from-the-client, which is the right shape for stub state — only
// the server agents should be writing here.

import { persistSet } from "./_persist.js";
import type { QualityDimension, QualityGrade, QualityScores } from "./agents.js";

const qualityKey = (appId: string) => `quality:${appId}`;

export interface CriticIssue {
  dimension: string;
  severity: string;
  note: string;
}

const KNOWN_DIMENSIONS = [
  "correctness",
  "security",
  "performance",
  "accessibility",
  "ux_completeness",
  "responsive",
  "code_health",
] as const;

const DIMENSION_LABELS: Record<string, string> = {
  correctness: "Correctness",
  security: "Security",
  performance: "Performance",
  accessibility: "Accessibility",
  ux_completeness: "UX completeness",
  responsive: "Responsive",
  code_health: "Code health",
};

/**
 * Map a per-dimension issue list to the current scoreboard mapping:
 *   0 issues          → A
 *   1 medium          → B
 *   1 high            → C
 *   1 critical / 2 high → D
 *   3+ critical       → F
 *
 * Anything in between picks the next worse grade. The aim is
 * "directional — looks right when the user spot-checks", not
 * statistical rigour.
 *
 * Exported so unit tests / future tools can assert the mapping
 * directly. NOT a public RPC because this module is underscore-
 * prefixed by design.
 */
export function gradeFromIssues(issues: CriticIssue[]): QualityGrade {
  let crit = 0;
  let high = 0;
  let med = 0;
  let low = 0;
  for (const i of issues) {
    const s = (i.severity || "").toLowerCase();
    if (s === "critical") crit += 1;
    else if (s === "high") high += 1;
    else if (s === "medium") med += 1;
    else low += 1;
  }
  if (crit >= 3) return "F";
  if (crit >= 1 || high >= 2) return "D";
  if (high === 1) return "C";
  if (med >= 1) return "B";
  if (low >= 2) return "B+";
  if (low === 1) return "A-";
  return "A";
}
gradeFromIssues.config = { id: "_internal.gradeFromIssues" };

const GRADE_RANK: Record<QualityGrade, number> = {
  "A+": 12, A: 11, "A-": 10,
  "B+": 9, B: 8, "B-": 7,
  "C+": 6, C: 5, "C-": 4,
  D: 3, F: 1,
};

function overallFrom(dims: QualityDimension[]): QualityGrade {
  // Worst-grade-wins. Composite via numeric average reads better but
  // hides single-dimension cliffs (one F should drag the headline
  // down). The chat receipt already shows per-dimension counts, so
  // the overall doesn't need to summarise — it needs to flag.
  let worst: QualityGrade = "A+";
  for (const d of dims) {
    if (GRADE_RANK[d.grade] < GRADE_RANK[worst]) worst = d.grade;
  }
  return worst;
}

/**
 * Persist a fresh scorecard derived from the Critic's per-dimension
 * issue list. Issues without a recognised dimension are dropped (so
 * a typo in the LLM output doesn't pollute the grid). Dimensions not
 * mentioned by the Critic this round inherit grade A (with a "no
 * issues raised this round" rationale) — the alternative (carry-over
 * from last round) would let stale grades linger; the spec frames the
 * scorecard as a per-deploy snapshot, so reset is correct.
 */
export async function setQualityFromCritic(
  appId: string,
  issues: CriticIssue[],
): Promise<void> {
  // Bucket issues by dimension.
  const byDim = new Map<string, CriticIssue[]>();
  for (const i of issues) {
    const key = (i.dimension || "").toLowerCase();
    if (!KNOWN_DIMENSIONS.includes(key as (typeof KNOWN_DIMENSIONS)[number])) {
      continue;
    }
    const list = byDim.get(key) ?? [];
    list.push(i);
    byDim.set(key, list);
  }
  const dimensions: QualityDimension[] = KNOWN_DIMENSIONS.map((k) => {
    const dimIssues = byDim.get(k) ?? [];
    const grade = gradeFromIssues(dimIssues);
    const rationale =
      dimIssues.length === 0
        ? "No issues raised this round."
        : dimIssues
            .slice(0, 2)
            .map((i) => i.note || `${i.severity} issue`)
            .join(" · ");
    return { key: k, label: DIMENSION_LABELS[k] ?? k, grade, rationale };
  });
  const scores: QualityScores = {
    overall: overallFrom(dimensions),
    last_run_at: new Date().toISOString(),
    dimensions,
  };
  await persistSet(qualityKey(appId), scores);
}
setQualityFromCritic.config = { id: "_internal.setQualityFromCritic" };
