# M0 Gate Tuning

Date: 2026-05-26

## Status

Accepted for the pre-launch M0 gate.

## Context

The first M0 subset showed two separate problems:

- Some prompts fast-failed before any tool call. A direct raw SSE capture for
  `todo-board` showed the response body contained only an AI SDK error frame:
  OpenAI returned `429` insufficient quota, followed by `[DONE]`.
- `markdown-notes` completed a real build/deploy attempt but Reviewer blocked
  it for a real XSS risk from `dangerouslySetInnerHTML` and a high-severity
  stale-state correctness bug.

## Decision

The M0 harness now persists raw SSE frames for each Builder chat turn under
`.zeroship/m0-gate/logs/` and includes the raw stream path in agent failure
messages. Empty `tools=` failures should carry enough evidence to distinguish
provider/runtime errors from a model that simply did not engage.

The Builder system prompt now gives explicit React guardrails:

- Do not use `dangerouslySetInnerHTML` for user-controlled or persisted content.
- Render markdown previews as React elements or plain text unless a vetted
  sanitizer is installed and verified.
- Use functional state setters and compute related state transitions from the
  same next-state value to avoid stale closures.

Reviewer severity policy remains unchanged: high and critical findings block
deploy; medium findings are warnings unless the Reviewer sets `approved=false`
because they combine with another hard blocker. This keeps the gate focused on
real ship safety without weakening the high-severity correctness bar.

## Consequences

The gate is still a real-path test and can fail on upstream OpenAI quota or
rate-limit errors. Those failures should now be reported as provider errors
with raw-stream evidence rather than ambiguous no-tool outcomes.
