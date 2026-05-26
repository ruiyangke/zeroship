"use server";
// PM SubAgent — chat-mode product manager. Helps creators decide what
// to build next based on existing project state (issues + roadmap +
// recent activity).
//
// Per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §11 + §4.8.3.2:
//   - PM has two modes:
//       · Conversational SubAgent (this file) — invoked when the user
//         asks "@pm what should I build next?" or when Builder routes
//         a strategic question via task("pm", { question }).
//       · Background scheduled worker — polls project state and posts
//         digests on a cron. That worker is OUT OF SCOPE here (per the
//         task brief: "chat-mode + scheduled stub"). The stub lives in
//         agents.ts as the seed issues/roadmap data the SubAgent reads.
//   - For V1 the PM SubAgent reads project state via the question text
//     (Builder summarises issues + roadmap + recent deploys in the
//     `question` arg). Wiring direct DB access through deepagents tools
//     is deferred — same model as Critic / Reviewer: no fs/exec, just
//     structured input → structured output.
//
// `@pm` chat-composer routing: the task brief flags this as a UX
// nicety to defer. V1 ships PM-as-task; Builder calls task("pm", …)
// when it detects a strategic-direction question.

import type { SubAgent } from "deepagents";
import { z } from "zod";

import { PM_PROMPT } from "./prompts.js";

// One recommendation slot — can be tied to an existing issue (issueId)
// or be a brand-new suggestion (issueId omitted). The card on the
// client renders `title` as the headline, `why` as the rationale, and
// `urgency` as a coloured pill (low → ink, medium → amber, high → ivy
// or red depending on positive/negative framing).
const recommendationSchema = z.object({
  // If present, references an existing Issue id (see agents.ts). If
  // omitted, this is a fresh suggestion that doesn't have a backlog
  // entry yet.
  issueId: z.string().optional(),
  title: z.string(),
  why: z.string(),
  urgency: z.enum(["low", "medium", "high"]),
});

export const pmResponseSchema = z.object({
  // The single most-recommended next thing. PM is opinionated by
  // design — V1 surfaces ONE primary recommendation, not a ranked
  // list, so creators don't get decision paralysis.
  recommendation: recommendationSchema,
  // Up to 2 alternatives so a creator who disagrees with the primary
  // pick has visible escape hatches. The card renders these collapsed
  // by default with a "see alternatives" disclosure.
  alternatives: z.array(recommendationSchema).max(2).default([]),
});

export type PMRecommendation = z.infer<typeof pmResponseSchema>;

export const pm: SubAgent = {
  name: "pm",
  description:
    "Strategic product-manager subagent. Builder calls task(\"pm\", { " +
    "question }) when the user asks 'what should I build next?' or " +
    "similar direction questions. Returns { recommendation: {issueId?, " +
    "title, why, urgency}, alternatives: [...] } — the single most-" +
    "recommended next move plus up to 2 alternatives. Tone: terse, " +
    "strategic, opinionated.",
  systemPrompt: PM_PROMPT,
  model: "openai:gpt-5.4-mini",
  // No tools — PM judges what Builder hands over (issues + roadmap +
  // recent activity summarised in the question). Direct DB access is
  // a follow-up; for V1 keep the surface tight.
  tools: [],
  responseFormat: pmResponseSchema,
};
