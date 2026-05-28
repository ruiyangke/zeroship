"use server";
// Critic SubAgent — reviews Builder's output across quality dimensions
// (correctness, security, performance, accessibility, ux_completeness,
// responsive, code_health). Returns structured feedback that Builder's
// planning loop checks; if not approved and iteration count < N, Builder
// revises.
//
// Per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §11.1 + §4.8.9:
//  - SubAgent (deepagents primitive), not a separate runtime
//  - responseFormat = Zod schema for structured output — deepagents
//    forwards this to LangChain's `createAgent`, which accepts a Zod
//    schema directly via `ResponseFormatInput` (see
//    node_modules/langchain/dist/agents/responses.d.ts).
//  - Model: openai:gpt-5.4-mini, same family as Builder. Standardised
//    across all subagents for now (model parity > cost diff in the
//    initial rollout; revisit if telemetry shows Critic spend dominates).
//  - Builder calls task("critic", { changes }) after each commit; the
//    "loop" is a plain JS while inside Builder's planning, not a custom
//    LangGraph cycle.
//
// No tools: Critic only reads what Builder hands over (a diff / change
// description) inside the task-tool input. It doesn't need fs/exec
// access — and giving it none keeps the scope tight and the cost low.

import type { SubAgent } from "deepagents";
import { z } from "zod";

import { CRITIC_PROMPT } from "./prompts";

export const criticResponseSchema = z.object({
  approved: z.boolean(),
  issues: z.array(
    z.object({
      dimension: z.enum([
        "correctness",
        "security",
        "performance",
        "accessibility",
        "ux_completeness",
        "responsive",
        "code_health",
      ]),
      severity: z.enum(["low", "medium", "high", "critical"]),
      issue: z.string(),
      suggested_fix: z.string(),
      line: z.number().int().optional(),
    }),
  ),
});

export type CriticResponse = z.infer<typeof criticResponseSchema>;

export const critic: SubAgent = {
  name: "critic",
  description:
    "Reviews Builder's recent code changes across 7 quality dimensions " +
    "(correctness, security, performance, accessibility, ux_completeness, " +
    "responsive, code_health) and returns structured approval / issues. " +
    "Called by Builder after each commit; iterate until approved or limit hit.",
  systemPrompt: CRITIC_PROMPT,
  // Same model family as Builder. Per-agent model tuning
  // deferred until cost telemetry justifies divergence.
  model: "openai:gpt-5.4-mini",
  // No tools — Critic reviews what Builder hands over via the task input.
  // Giving Critic fs/exec would invite scope creep (it'd start running
  // tests itself) and cost more per loop. If a future review dimension
  // needs codebase access, prefer extending the input shape, not the
  // tool list.
  tools: [],
  responseFormat: criticResponseSchema,
};
