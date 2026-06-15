"use server";

import { describe, expect, it } from "vitest";

import { qualityScoresFromCriticIssues } from "./agent-writes";
import { criticResponseSchema } from "./critic";
import { BUILDER_SYSTEM, CRITIC_PROMPT } from "./prompts";
import {
  CRITIC_DIMENSIONS,
  REVIEWER_BLOCKER_KINDS,
} from "../../shared/review-contract";
import {
  normalizeReviewerGate,
  reviewerHasHardBlockers,
  reviewerResponseSchema,
} from "./reviewer";

describe("Phase 3 moat prompt contract", () => {
  it("Builder prompt instructs generated React apps to compose from @zeroship/ui", () => {
    for (const required of [
      "@zeroship/ui",
      "@zeroship/ui/styles.css",
      "ThemeProvider",
      "Button",
      "Card",
      "Dialog",
      "Select",
      "Tabs",
      "Input",
      "Textarea",
      "Table",
      "Badge",
      "Toast",
      "EmptyState",
      "--zs-*",
    ]) {
      expect(BUILDER_SYSTEM).toMatch(new RegExp(required.replace("*", "\\*")));
    }
  });
});

describe("Phase 3 moat Critic dimensions", () => {
  it("schema accepts every Critic dimension named in the review contract", () => {
    for (const dimension of CRITIC_DIMENSIONS) {
      const parsed = criticResponseSchema.parse({
        approved: false,
        issues: [
          {
            dimension,
            severity: "medium",
            issue: `Sample ${dimension} issue`,
            suggested_fix: "Fix the sample issue.",
          },
        ],
      });
      expect(parsed.issues[0]?.dimension).toBe(dimension);
      expect(CRITIC_PROMPT).toMatch(new RegExp(dimension));
    }
  });

  it("bad and clean samples map into scorecard grades without OpenAI", () => {
    const badScores = qualityScoresFromCriticIssues(
      [
        {
          dimension: "states",
          severity: "critical",
          note: "Missing error state leaves users stuck after a failed save.",
        },
      ],
      "2026-05-26T00:00:00.000Z",
    );
    expect(badScores.overall).toBe("D");
    expect(badScores.dimensions.find((d) => d.key === "states")?.grade).toBe("D");
    expect(badScores.dimensions.find((d) => d.key === "states")?.rationale ?? "")
      .toMatch(/Missing error state/);

    const cleanScores = qualityScoresFromCriticIssues(
      [],
      "2026-05-26T00:00:00.000Z",
    );
    expect(cleanScores.overall).toBe("A");
    expect(cleanScores.dimensions.length).toBe(CRITIC_DIMENSIONS.length);
    expect(cleanScores.dimensions.every((d) => d.grade === "A")).toBe(true);
  });
});

describe("Phase 3 moat Reviewer blocker kinds", () => {
  it("schema accepts every Reviewer blocker kind named in the review contract", () => {
    for (const kind of REVIEWER_BLOCKER_KINDS) {
      const parsed = reviewerResponseSchema.parse({
        approved: false,
        blockers: [
          {
            kind,
            severity: "high",
            why: `Sample ${kind} blocker`,
            fix: "Fix the sample blocker.",
          },
        ],
      });
      expect(parsed.blockers[0]?.kind).toBe(kind);
    }
  });

  it("hard blockers block deploy, medium warnings do not, clean passes", () => {
    const bad = reviewerResponseSchema.parse({
      approved: true,
      blockers: [
        {
          kind: "dangerous_html_user_content",
          severity: "critical",
          why: "User-authored markdown is rendered through dangerouslySetInnerHTML.",
          fix: "Render a safe markdown subset as React elements.",
        },
      ],
    });
    expect(reviewerHasHardBlockers(bad)).toBe(true);
    expect(normalizeReviewerGate(bad).approved).toBe(false);

    const warning = reviewerResponseSchema.parse({
      approved: false,
      blockers: [
        {
          kind: "missing_critical_states",
          severity: "medium",
          why: "The non-critical archive panel lacks an empty state.",
        },
      ],
    });
    expect(reviewerHasHardBlockers(warning)).toBe(false);
    expect(normalizeReviewerGate(warning).approved).toBe(true);

    const clean = reviewerResponseSchema.parse({
      approved: true,
      blockers: [],
    });
    expect(normalizeReviewerGate(clean).approved).toBe(true);
  });
});
