import { test, expect } from "@playwright/test";

// Multi-agent fleet smoke test. Reviewer / PM / SRE are V1
// SubAgents wired into Builder via deepagents. Each is invoked when
// Builder hits a matching prompt:
//   - "deploy / review" → task("reviewer", …) → data-reviewer-round
//   - "what should I build next?" → task("pm", …) → data-pm-recommendation
//   - "why is the app slow?" → task("sre", …) → data-sre-finding
//
// Gating mirrors critic-loop.spec.ts:
//   - OPENAI_API_KEY required.
//   - Sandbox controller reachable (Builder's tools route through it).
//   - Control plane is NOT required — uses the workspace catchall route.
//
// These tests assert ONLY that the cards render (visible + correct
// testid). The structured payload's content is non-deterministic
// (real LLM); we keep assertions loose so the suite isn't flaky on
// model wording.

const HAS_KEY = !!process.env.OPENAI_API_KEY;
// Each subagent dispatch is one extra round-trip on top of the chat
// turn — budget similar to critic-loop.spec.
const STEP_TIMEOUT = 90_000;

const SHELL_PATH = "/__catchall_for_test";

async function probe(url: string): Promise<boolean> {
  try {
    const res = await fetch(url, { signal: AbortSignal.timeout(1500) });
    return res.ok || res.status === 401 || res.status === 404;
  } catch {
    return false;
  }
}

test.describe("multi-agent fleet (Reviewer / PM / SRE)", () => {
  test.beforeEach(async ({}, testInfo) => {
    testInfo.setTimeout(STEP_TIMEOUT + 30_000);
  });

  test("Reviewer: deploy-shaped prompt → ReviewerRoundCard renders", async ({
    page,
  }) => {
    test.skip(!HAS_KEY, "OPENAI_API_KEY not set — skipping real-LLM test");
    const sandboxUrl = process.env.SANDBOX_URL ?? "http://localhost:9091";
    const sandboxUp = await probe(`${sandboxUrl}/health`);
    test.skip(
      !sandboxUp,
      `sandbox controller unreachable at ${sandboxUrl} — skipping`,
    );

    await page.goto(SHELL_PATH);
    await expect(page.getByTestId("chat-rail")).toBeVisible();

    // Engineered prompt: nudges Builder to call task("reviewer", …)
    // explicitly. The BUILDER_SYSTEM directive tells it to review
    // before deploys; we make the deploy intent explicit.
    const prompt =
      "I want to deploy the current app. Before you do, run a " +
      'review pass — call task("reviewer", …) with a concise summary ' +
      "of the (empty) current state of the project. Do not write any " +
      "files. Just trigger the reviewer subagent.";

    await page.getByTestId("chat-input").fill(prompt);
    await page.getByTestId("chat-input").press("Control+Enter");

    const card = page.getByTestId("reviewer-round-card").first();
    await expect(card).toBeVisible({ timeout: STEP_TIMEOUT });
    // Header text is stable across approved / blocked variants.
    await expect(card).toContainText(/reviewer/i);
  });

  test("PM: 'what should I build next?' → PMRecommendationCard renders", async ({
    page,
  }) => {
    test.skip(!HAS_KEY, "OPENAI_API_KEY not set — skipping real-LLM test");
    const sandboxUrl = process.env.SANDBOX_URL ?? "http://localhost:9091";
    const sandboxUp = await probe(`${sandboxUrl}/health`);
    test.skip(
      !sandboxUp,
      `sandbox controller unreachable at ${sandboxUrl} — skipping`,
    );

    await page.goto(SHELL_PATH);
    await expect(page.getByTestId("chat-rail")).toBeVisible();

    const prompt =
      "I have a brand-new project with no code yet. What should I " +
      'build next? Route this strategic question through task("pm", …) ' +
      "so the PM subagent can recommend a starting point.";

    await page.getByTestId("chat-input").fill(prompt);
    await page.getByTestId("chat-input").press("Control+Enter");

    const card = page.getByTestId("pm-recommendation-card").first();
    await expect(card).toBeVisible({ timeout: STEP_TIMEOUT });
    await expect(card).toContainText(/pm/i);
  });

  test("SRE: 'why is the app slow?' → SREFindingCard renders", async ({
    page,
  }) => {
    test.skip(!HAS_KEY, "OPENAI_API_KEY not set — skipping real-LLM test");
    const sandboxUrl = process.env.SANDBOX_URL ?? "http://localhost:9091";
    const sandboxUp = await probe(`${sandboxUrl}/health`);
    test.skip(
      !sandboxUp,
      `sandbox controller unreachable at ${sandboxUrl} — skipping`,
    );

    await page.goto(SHELL_PATH);
    await expect(page.getByTestId("chat-rail")).toBeVisible();

    // Sharpened to force the dispatch — the model otherwise sometimes
    // self-diagnoses with `ls` / `glob` instead of routing the question.
    // Explicit "do not run any other tools first" + the literal call
    // signature kept Builder honest in 5/5 retries during dev.
    const prompt =
      "Users say the dashboard feels slow — pages take ~3 seconds to " +
      "load. Your FIRST tool call must be " +
      'task({ subagent_type: "sre", description: "diagnose dashboard slowness, recommend a fix" }). ' +
      "Do not call ls, glob, write_todos, or any other tool before " +
      "the SRE subagent has returned. After it returns, you may " +
      "summarise its finding for the user.";

    await page.getByTestId("chat-input").fill(prompt);
    await page.getByTestId("chat-input").press("Control+Enter");

    const card = page.getByTestId("sre-finding-card").first();
    await expect(card).toBeVisible({ timeout: STEP_TIMEOUT });
    await expect(card).toContainText(/sre/i);
  });
});
