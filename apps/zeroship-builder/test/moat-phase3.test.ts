import assert from "node:assert/strict";
import { describe, test } from "node:test";

import { qualityScoresFromCriticIssues } from "../src/server/internal/agent-writes.js";
import { criticResponseSchema } from "../src/server/internal/critic.js";
import { BUILDER_SYSTEM, CRITIC_PROMPT } from "../src/server/internal/prompts.js";
import {
  CRITIC_DIMENSIONS,
  REVIEWER_BLOCKER_KINDS,
} from "../src/shared/review-contract.js";
import {
  normalizeReviewerGate,
  reviewerHasHardBlockers,
  reviewerResponseSchema,
} from "../src/server/internal/reviewer.js";

describe("Phase 3 moat prompt contract", () => {
  test("Builder prompt instructs generated React apps to compose from @zeroship/ui", () => {
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
      assert.match(BUILDER_SYSTEM, new RegExp(required.replace("*", "\\*")));
    }
  });
});

describe("Phase 3 moat Critic dimensions", () => {
  test("schema accepts every Critic dimension named in the review contract", () => {
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
      assert.equal(parsed.issues[0]?.dimension, dimension);
      assert.match(CRITIC_PROMPT, new RegExp(dimension));
    }
  });

  test("bad and clean samples map into scorecard grades without OpenAI", () => {
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
    assert.equal(badScores.overall, "D");
    assert.equal(
      badScores.dimensions.find((d) => d.key === "states")?.grade,
      "D",
    );
    assert.match(
      badScores.dimensions.find((d) => d.key === "states")?.rationale ?? "",
      /Missing error state/,
    );

    const cleanScores = qualityScoresFromCriticIssues(
      [],
      "2026-05-26T00:00:00.000Z",
    );
    assert.equal(cleanScores.overall, "A");
    assert.equal(cleanScores.dimensions.length, CRITIC_DIMENSIONS.length);
    assert.ok(cleanScores.dimensions.every((d) => d.grade === "A"));
  });
});

describe("Phase 3 moat Reviewer blocker kinds", () => {
  test("schema accepts every Reviewer blocker kind named in the review contract", () => {
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
      assert.equal(parsed.blockers[0]?.kind, kind);
    }
  });

  test("hard blockers block deploy, medium warnings do not, clean passes", () => {
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
    assert.equal(reviewerHasHardBlockers(bad), true);
    assert.equal(normalizeReviewerGate(bad).approved, false);

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
    assert.equal(reviewerHasHardBlockers(warning), false);
    assert.equal(normalizeReviewerGate(warning).approved, true);

    const clean = reviewerResponseSchema.parse({
      approved: true,
      blockers: [],
    });
    assert.equal(normalizeReviewerGate(clean).approved, true);
  });
});
