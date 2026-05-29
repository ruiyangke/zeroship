"use server";
// SRE SubAgent — chat-mode site-reliability engineer. Diagnoses health
// issues (errors, slowdowns, broken probes) using app logs + perf data
// + status surfaced via the question text.
//
// Per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §11 + §4.8.3.2:
//   - Like PM, SRE has two modes:
//       · Conversational SubAgent (this file) — invoked when the user
//         asks "@sre why is the app slow?" or Builder routes a
//         diagnostic question via task("sre", { question }).
//       · Background scheduled worker — cron-driven monitoring,
//         post-deploy verification, auto-rollback (§11.3 / §11.4).
//         OUT OF SCOPE here (task brief: "chat-mode + scheduled
//         stub"). The stub lives in agents.ts as the seed quality-
//         scorecard data.
//   - V1 reads health context via the `question` arg (Builder
//     summarises logs / perf / recent incidents). Direct log-stream
//     access is deferred — same shape as Critic / Reviewer / PM.
//
// `@sre` chat-composer routing: deferred UX nicety. V1 ships SRE-as-
// task; Builder calls task("sre", …) when it detects a diagnostic /
// reliability question.

import type { SubAgent } from "deepagents";
import { z } from "zod";

import { SRE_PROMPT } from "./prompts";

export const sreResponseSchema = z.object({
  // Plain-text root cause as best as can be determined from the
  // evidence in `question`. SRE is allowed to say "insufficient data
  // — enable structured logging on /api/foo and try again" — that's
  // a valid diagnosis for V1 where the SubAgent doesn't pull logs
  // itself.
  diagnosis: z.string(),
  // Severity drives the card's colour ramp:
  //   info     → ink (just FYI)
  //   warning  → amber (degraded but live)
  //   error    → ivy (broken for some users)
  //   critical → red (broken for all users / data loss)
  severity: z.enum(["info", "warning", "error", "critical"]),
  // What to do about it. Should be specific and actionable
  // ("increase DB pool from 5 → 20 in env tab"; not "investigate
  // performance"). The client renders this as the card's primary
  // body.
  recommendation: z.string(),
  // Optional log excerpts SRE pulled from the question to ground its
  // diagnosis. Each entry is a short snippet (one log line + a
  // pointer where it came from). Rendered as a small <pre> block
  // under the recommendation when present.
  related_logs: z
    .array(
      z.object({
        source: z.string(),
        excerpt: z.string(),
      }),
    )
    .max(5)
    .optional(),
});

export type SREFinding = z.infer<typeof sreResponseSchema>;

export const sre: SubAgent = {
  name: "sre",
  description:
    "Diagnostic subagent for app health and reliability. Builder calls " +
    "task(\"sre\", { question }) when the user asks 'why is the app " +
    "slow / erroring / down?' or similar reliability questions. Returns " +
    "{ diagnosis, severity, recommendation, related_logs? } — calm " +
    "postmortem tone, specific and actionable. Severity in { info, " +
    "warning, error, critical }.",
  systemPrompt: SRE_PROMPT,
  model: "openai:gpt-5.4-mini",
  // No tools — SRE judges what Builder hands over via the question.
  // Direct log/metrics access is a follow-up.
  tools: [],
  responseFormat: sreResponseSchema,
};
