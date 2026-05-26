"use server";
// Reviewer schema + SubAgent. The deploy hard gate invokes
// REVIEWER_PROMPT + reviewerResponseSchema directly from the deploy
// tool so the approval check cannot be bypassed by prompt sequencing.
// The registered SubAgent remains available for non-deploy destructive
// reviews.
//
// Per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §11 (role table) + §11.2 (pre-deploy gate matrix) + §4.8.3.2
// (deepagents fleet mapping):
//
//   - SubAgent (one-shot, like Critic but lighter — no iteration loop).
//   - The deploy tool calls the Reviewer model before any deploy. Builder
//     calls task("reviewer", { changes }) only for destructive non-deploy
//     operations. If approved=false, Builder fixes the listed blockers
//     and re-calls; if it can't, it escalates to the user.
//   - Reviewer's hard-gate dimensions are the §11.2 matrix:
//       · build / typecheck pass
//       · no secrets in client bundle
//       · auth bypass / SQL injection / XSS in changed code
//       · no dangerouslySetInnerHTML on user content
//       · critical UI states and serious accessibility violations
//       · destructive migration safety (drop column, truncate, prod env tweak)
//       · code-and-migration coupling (schema change ships with the code that uses it)
//   - Spec §11 also envisions Reviewer wired via deepagents `interruptOn`
//     for true human-in-the-loop hard gates (e.g., destructive prod
//     migration). Per the task brief that integration is DEFERRED — we
//     ship the deploy-tool hard gate first and revisit `interruptOn` once
//     the destructive-op tooling exists.
//
// No tools: same rationale as Critic — Reviewer reviews the diff/change
// description Builder hands over via the task input. If a future check
// needs codebase access, prefer extending the input shape over giving
// the SubAgent fs/exec.

import type { SubAgent } from "deepagents";
import { z } from "zod";

import { REVIEWER_PROMPT } from "./prompts.js";
import {
  REVIEWER_BLOCKER_KINDS,
  REVIEWER_HARD_GATE_SEVERITIES,
} from "../../shared/review-contract.js";

export const reviewerResponseSchema = z.object({
  approved: z.boolean(),
  blockers: z.array(
    z.object({
      kind: z.enum(REVIEWER_BLOCKER_KINDS),
      // Severity mirrors Critic so the client can render the same
      // colour scheme. Reviewer's bar is higher — anything "high" or
      // "critical" should set approved=false.
      severity: z.enum(["low", "medium", "high", "critical"]),
      why: z.string(),
      fix: z.string().optional(),
    }),
  ),
});

export type ReviewerResponse = z.infer<typeof reviewerResponseSchema>;

export function reviewerHasHardBlockers(review: ReviewerResponse): boolean {
  return review.blockers.some((blocker) =>
    REVIEWER_HARD_GATE_SEVERITIES.includes(
      blocker.severity as (typeof REVIEWER_HARD_GATE_SEVERITIES)[number],
    ),
  );
}

export function normalizeReviewerGate(review: ReviewerResponse): ReviewerResponse {
  return {
    ...review,
    approved: !reviewerHasHardBlockers(review),
  };
}

export const reviewer: SubAgent = {
  name: "reviewer",
  description:
    "Manual hard-gate review for destructive operations that are not " +
    "deploys. Deploys must use the deploy tool, which invokes this " +
    "same prompt/schema internally before it can upload. Reviews changes " +
    "for security (secrets in client, auth bypass, injection, XSS), " +
    "UI hard gates (critical states, serious a11y), correctness " +
    "(build / typecheck status, smoke tests), and " +
    "destructive-op safety (migrations dropping data, force-pushes, prod " +
    "env tweaks). Returns { approved, blockers: [{kind, severity, why, " +
    "fix?}] }.",
  systemPrompt: REVIEWER_PROMPT,
  // Keep the same model family across the fleet for now. Reviewer's
  // workload is similar in shape to Critic's (one-shot structured
  // review of a code diff), so model parity is the right starting
  // point.
  model: "openai:gpt-5.4-mini",
  // No tools — Reviewer judges what Builder hands over via the task
  // input. See header comment for the rationale.
  tools: [],
  responseFormat: reviewerResponseSchema,
};
