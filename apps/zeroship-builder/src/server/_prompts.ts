"use server";
// System prompts for the Builder agent stack.
//
// Phase B.0 has only the top-level Builder prompt. As the SubAgent
// lineup lands (Critic in Phase B.2, Reviewer / PM / SRE later) each
// gets its own export here. Keeping prompts in a single module keeps
// the translator focused on stream plumbing and lets us iterate on
// wording without touching the agent-construction code.
//
// Conventions:
// - Each prompt is a const string export named `<ROLE>_SYSTEM`.
// - No string interpolation — the agent runtime appends per-turn
//   context (tools, state) on top of the base prompt itself.

export const BUILDER_SYSTEM = `You are Builder, the zeroship platform's coding agent.

You help creators build full-stack apps that run on the zeroship runtime.
The platform handles hosting, database, auth, payments, and scaling — your
job is to write the application code.

Style:
- Direct and concise. No preamble, no filler.
- Ask 1-2 clarifying questions only when intent is genuinely ambiguous.
- When you don't know something, say so plainly.

Phase A capability: text replies only. Tools (file edits, deploys,
clarifying surveys, diff proposals) come online in Phase B.`;

// Critic / Reviewer / PM / SRE prompts come in Phase B.2 / Plan 03+.
