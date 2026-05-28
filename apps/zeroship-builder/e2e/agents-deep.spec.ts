import { test, expect } from "@playwright/test";

// Deep e2e for the multi-agent fleet (Reviewer / PM / SRE) — three
// SubAgents wired into Builder via deepagents. Unlike multi-agent.spec
// (which is a smoke test) this file exercises richer prompts that
// emulate real creator flows: pre-deploy review, strategic
// recommendation, and SRE health diagnosis. Gated on full infra.

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

test.describe("Multi-agent fleet end-to-end (real LLM)", () => {
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

  test("Reviewer pre-deploy gate", async ({ page }, testInfo) => {
    testInfo.setTimeout(180_000);
    await page.goto(SHELL_PATH);
    await expect(page.getByTestId("chat-rail")).toBeVisible();
    await page
      .getByTestId("chat-input")
      .fill(
        "Write /workspace/Hello.tsx with a simple hello-world component, " +
          "then call task('reviewer', ...) to review it before I deploy.",
      );
    await page.getByTestId("chat-input").press("Control+Enter");
    await expect(page.getByTestId("reviewer-round-card").first()).toBeVisible({
      timeout: 180_000,
    });
  });

  test("PM recommends next step", async ({ page }, testInfo) => {
    testInfo.setTimeout(180_000);
    await page.goto(SHELL_PATH);
    await expect(page.getByTestId("chat-rail")).toBeVisible();
    await page.getByTestId("chat-input").fill("@pm what should I build next?");
    await page.getByTestId("chat-input").press("Control+Enter");
    await expect(
      page.getByTestId("pm-recommendation-card").first(),
    ).toBeVisible({ timeout: 180_000 });
  });

  test("SRE diagnoses health concern", async ({ page }, testInfo) => {
    testInfo.setTimeout(180_000);
    await page.goto(SHELL_PATH);
    await expect(page.getByTestId("chat-rail")).toBeVisible();
    await page
      .getByTestId("chat-input")
      .fill("@sre why might my app be slow in production?");
    await page.getByTestId("chat-input").press("Control+Enter");
    await expect(page.getByTestId("sre-finding-card").first()).toBeVisible({
      timeout: 180_000,
    });
  });
});
