"use server";

import { describe, expect, it } from "vitest";

import { BUILDER_SYSTEM } from "./prompts";

// The console is a PURE creator app: there is no deploy/ship/publish path.
// chat.ts registers exactly `[askSurveyTool, reviewTool]`, so the agent's
// quality tool is named `review` and there is NO tool named `deploy`. The
// system prompt and the runtime tool set MUST stay coherent — a prompt that
// advertises a `deploy` tool the model can't call (or that hides the real
// `review` tool) is a broken LLM-facing contract.
//
// See docs/superpowers/specs/2026-05-31-console-pure-creator-app-design.md
// (Work breakdown A: "tools.ts deploy tool → reviewer-only").
//
// These assertions FAIL against the pre-change prompt, which still told the
// model to "deploy the current app (`deploy`)" and to "call `deploy`
// instead" of `task("reviewer", ...)`.
describe("BUILDER_SYSTEM — prompt ↔ tool-name alignment", () => {
  it("describes the `review` tool", () => {
    expect(BUILDER_SYSTEM).toMatch(/`review`/);
    // The review tool ships nothing — the prompt must say so, not promise a
    // build/upload.
    expect(BUILDER_SYSTEM).toMatch(/ships? NOTHING/i);
  });

  it("does NOT instruct the model to call a `deploy` tool", () => {
    // No backticked `deploy` tool reference (the only on-prompt deploy
    // mentions must be the explicit "there is no deploy" disclaimers, which
    // are unbackticked prose).
    expect(BUILDER_SYSTEM).not.toMatch(/`deploy`/);
    // And it must not advertise the removed `.zship` upload artifact as a
    // thing the agent produces.
    expect(BUILDER_SYSTEM).not.toMatch(/uploads? the resulting/i);
  });

  it("states there is no platform deploy in the console", () => {
    expect(BUILDER_SYSTEM).toMatch(/no deploy/i);
  });
});
