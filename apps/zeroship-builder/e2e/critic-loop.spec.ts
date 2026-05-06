import { test, expect } from "@playwright/test";

// After a write, Builder calls `task("critic", …)`
// batch and the v6 stream surfaces a `data-critic-round` chunk that
// the client renders as a CriticRoundCard. This smoke test drives a
// real Builder turn that is engineered to require at least one
// write_file (so the critic-loop directive in BUILDER_SYSTEM kicks
// in) and asserts the badge appears.
//
// Gating mirrors spine-openai.spec.ts:
//   - OPENAI_API_KEY required.
//   - Sandbox controller reachable (Builder's write_file tool routes
//     through it).
//   - Control plane is NOT required — we use the workspace catchall
//     route, same as chat-openai.spec.ts.

const HAS_KEY = !!process.env.OPENAI_API_KEY;
// Critic is one extra round-trip per write, so budget is wider than
// chat-openai's plain stream test.
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

test.describe("critic loop wires into the chat stream", () => {
  test("Builder writes a file → task(\"critic\") → CriticRoundCard renders", async ({
    page,
  }, testInfo) => {
    test.skip(!HAS_KEY, "OPENAI_API_KEY not set — skipping real-LLM test");
    // Builder turn + write_file + critic dispatch — three real LLM
    // round-trips. Default 30s outer budget would expire before the
    // critic round-trip even starts; raise it to twice STEP_TIMEOUT.
    testInfo.setTimeout(STEP_TIMEOUT * 2 + 30_000);
    const sandboxUrl = process.env.SANDBOX_URL ?? "http://localhost:9091";
    const sandboxUp = await probe(`${sandboxUrl}/health`);
    test.skip(
      !sandboxUp,
      `sandbox controller unreachable at ${sandboxUrl} — skipping`,
    );

    await page.goto(SHELL_PATH);
    await expect(page.getByTestId("chat-rail")).toBeVisible();

    // The prompt is engineered to:
    //  1. Be small enough that the LLM completes in one turn.
    //  2. REQUIRE a write_file (we ask for a real .tsx file).
    //  3. Be a "coherent slice" so the BUILDER_SYSTEM critic directive
    //     fires (a small component is a slice — a one-line edit isn't).
    const prompt =
      'Create a React component at /workspace/HelloButton.tsx that ' +
      'exports `HelloButton` and renders a <button> with the text "Hello". ' +
      "Use TypeScript. After writing, you must call task(\"critic\", …) " +
      "to review. Keep the component very short (under 15 lines).";

    await page.getByTestId("chat-input").fill(prompt);
    await page.getByTestId("chat-input").press("Control+Enter");

    // CriticRoundCard renders inside the assistant message bubble. The
    // testid is set on the card itself (see CriticRoundCard.tsx).
    const criticCard = page.getByTestId("critic-round-card").first();
    await expect(criticCard).toBeVisible({ timeout: STEP_TIMEOUT });
    // Badge text format: "critic round N/M" + status.
    await expect(criticCard).toContainText(/critic round/i);
  });
});
