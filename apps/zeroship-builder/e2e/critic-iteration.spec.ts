import { test, expect } from "@playwright/test";

// Deep e2e for the Critic iteration loop. Unlike critic-loop.spec.ts,
// this test exercises Critic on intentionally-flawed code so the round
// surfaces real findings (not a green pass). Gated on full infra
// (control plane + sandbox + OPENAI_API_KEY).

const HAS_KEY = !!process.env.OPENAI_API_KEY;

const SHELL_PATH = "/__test/workspace";

async function probeUrl(url: string): Promise<boolean> {
  try {
    const r = await fetch(url, { signal: AbortSignal.timeout(2000) });
    return r.ok || r.status === 401 || r.status === 404;
  } catch {
    return false;
  }
}

test.describe("Critic iteration loop (real LLM)", () => {
  test.beforeAll(async () => {
    const sandboxUrl = process.env.SANDBOX_URL ?? "http://localhost:9091";
    const controlUrl = process.env.CONTROL_URL ?? "http://localhost:9090";
    const sandboxUp = await probeUrl(`${sandboxUrl}/health`);
    const controlUp = await probeUrl(`${controlUrl}/health`);
    test.skip(
      !HAS_KEY || !sandboxUp || !controlUp,
      `gating: HAS_KEY=${HAS_KEY} sandbox=${sandboxUp} control=${controlUp}`,
    );
  });

  test("Builder writes flawed code → Critic flags → Builder fixes", async ({
    page,
  }, testInfo) => {
    testInfo.setTimeout(300_000);

    await page.goto(SHELL_PATH);
    await expect(page.getByTestId("chat-rail")).toBeVisible();

    const composer = page.getByTestId("chat-input");
    await composer.fill(
      "Write /workspace/Bad.tsx — a React component using `var` for loops, " +
        "no `key` prop on a list .map(), and a hardcoded password " +
        "`var pw = 'secret123';`. After writing, call task('critic', ...) " +
        "to review your work.",
    );
    await composer.press("Control+Enter");

    // Wait for at least one critic round to surface.
    await expect(page.getByTestId("critic-round-card").first()).toBeVisible({
      timeout: 180_000,
    });
  });
});
