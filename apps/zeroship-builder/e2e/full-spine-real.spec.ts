import { test, expect } from "@playwright/test";

// Plan 03.1+04 — full real-LLM spine smoke test.
//
// Drives the entire creator flow with no mocks:
//   1. /new → enter idea → Begin
//   2. survey → first option → submit (loop until brief)
//   3. brief → "Begin →" → createApp (control plane) → /p/<id>/preview
//   4. ChatRail seeded with the brief synthesis → Builder turn
//   5. Builder writes a file (we engineer the prompt to require one)
//   6. Files pill click → file appears in tree → click → content shows
//   7. Critic round badge surfaces inside the assistant bubble
//
// Gates on ALL THREE: OPENAI_API_KEY + sandbox controller (:9091) +
// control plane (:9090). Skips cleanly when any one is missing — the
// suite stays green on a stock dev box.

const HAS_KEY = !!process.env.OPENAI_API_KEY;

// Each step is a real LLM round-trip (decide × N + finalize +
// createApp + first Builder turn + write_file + critic dispatch).
// The wall budget mirrors `spine-openai.spec.ts` but is wider to
// cover the additional Files-canvas + Critic verification.
const STEP_TIMEOUT = 90_000;

async function probe(url: string): Promise<boolean> {
  try {
    const res = await fetch(url, { signal: AbortSignal.timeout(1500) });
    return res.ok || res.status === 401 || res.status === 404;
  } catch {
    return false;
  }
}

test.describe("Full spine — real LLM, real sandbox, real control plane", () => {
  test("wizard → createApp → workspace → file write → Files canvas → Critic round", async ({
    page,
  }, testInfo) => {
    test.skip(!HAS_KEY, "OPENAI_API_KEY not set — skipping full-spine test");
    const controlUrl = process.env.CONTROL_URL ?? "http://localhost:9090";
    const sandboxUrl = process.env.SANDBOX_URL ?? "http://localhost:9091";
    const [controlUp, sandboxUp] = await Promise.all([
      probe(`${controlUrl}/health`),
      probe(`${sandboxUrl}/health`),
    ]);
    test.skip(
      !controlUp,
      `control plane unreachable at ${controlUrl} — skipping`,
    );
    test.skip(
      !sandboxUp,
      `sandbox controller unreachable at ${sandboxUrl} — skipping`,
    );

    // 8 round-trips × STEP_TIMEOUT, plus generous slack for dev-server
    // re-bundles on cold isolates.
    testInfo.setTimeout(STEP_TIMEOUT * 8);

    // 1. Wizard.
    await page.goto("/new");
    await expect(page.getByTestId("wizard-prompt")).toBeVisible();
    await page
      .getByTestId("wizard-prompt")
      .fill(
        "Build a tip calculator that splits unevenly between people. " +
          "When you start coding, write the first React component to " +
          "/workspace/TipCalculator.tsx. After writing, run " +
          'task("critic", …) for review.',
      );
    await page.getByTestId("wizard-send-idea").click();

    const survey = page.getByTestId("survey-card");
    const brief = page.getByTestId("brief-card");
    await Promise.race([
      survey.waitFor({ state: "visible", timeout: STEP_TIMEOUT }),
      brief.waitFor({ state: "visible", timeout: STEP_TIMEOUT }),
    ]);

    // 2. Loop — answer each survey, click first option, submit.
    let activeSurveyIndex = 0;
    for (let i = 0; i < 6; i++) {
      if (await brief.isVisible()) break;
      const allSurveysHandle = page.getByTestId("survey-card");
      const anySurvey = allSurveysHandle.nth(activeSurveyIndex);
      const winner = await Promise.race([
        anySurvey
          .waitFor({ state: "visible", timeout: STEP_TIMEOUT })
          .then(() => "survey" as const),
        brief
          .waitFor({ state: "visible", timeout: STEP_TIMEOUT })
          .then(() => "brief" as const),
      ]);
      if (winner === "brief") break;
      const questions = await anySurvey.locator(".space-y-3 > div").all();
      for (const q of questions) {
        const firstOption = q.locator("button").first();
        if (await firstOption.isVisible()) {
          await firstOption.click();
        }
      }
      await anySurvey.getByRole("button", { name: /send/i }).click();
      activeSurveyIndex += 1;
      await Promise.race([
        page
          .getByTestId("survey-card")
          .nth(activeSurveyIndex)
          .waitFor({ state: "visible", timeout: STEP_TIMEOUT }),
        brief.waitFor({ state: "visible", timeout: STEP_TIMEOUT }),
      ]);
      await page.waitForTimeout(500);
    }

    await expect(brief).toBeVisible({ timeout: STEP_TIMEOUT });

    // 3. Begin — fires createApp + navigates to /p/<id>/preview.
    await expect(page.getByTestId("brief-begin")).toBeEnabled();
    await page.getByTestId("brief-begin").click();
    await expect(page).toHaveURL(/\/p\/[^/]+\/preview/, {
      timeout: STEP_TIMEOUT,
    });

    // 4. Workspace shell + chat rail.
    await expect(page.getByTestId("topbar-project")).toBeVisible({
      timeout: STEP_TIMEOUT,
    });
    await expect(page.getByTestId("chat-rail")).toBeVisible();

    // 5. Seeded user msg from the brief — ChatRail's mount-time
    // useEffect calls sendMessage("Brief from the wizard:\n\n…"), so
    // this is the first surface assertion the spine reaches the
    // workspace cleanly. ASSERT THIS BEFORE switching to any other
    // canvas — switching panels can race with the seeded turn's first
    // chunk arrival.
    const seeded = page.getByTestId("msg-user").first();
    await expect(seeded).toBeVisible({ timeout: STEP_TIMEOUT });
    await expect(seeded).toContainText("Brief from the wizard");

    const assistant = page.getByTestId("msg-assistant").last();
    await expect(assistant).toBeVisible({ timeout: STEP_TIMEOUT });
    await expect(assistant).not.toBeEmpty({ timeout: STEP_TIMEOUT });

    // 6. Critic round badge — the assistant reply should now contain
    // a CriticRoundCard (the prompt explicitly asks for one).
    const critic = page.getByTestId("critic-round-card").first();
    await expect(critic).toBeVisible({ timeout: STEP_TIMEOUT });

    // 7. Files canvas — Builder wrote at least one file during the
    // assistant turn. Click the Files pill and wait for the tree to
    // surface a file-tree node.
    await page.getByTestId("pill:files").click();
    await expect(page.getByTestId("files-canvas")).toBeVisible({
      timeout: STEP_TIMEOUT,
    });
    const anyTreeItem = page.locator('[data-testid^="file-tree-item:"]');
    await anyTreeItem.first().waitFor({
      state: "visible",
      timeout: STEP_TIMEOUT,
    });

    // 8. Click the first file → viewer pane shows content.
    await anyTreeItem.first().click();
    await expect(page.getByTestId("files-viewer")).toBeVisible({
      timeout: STEP_TIMEOUT,
    });
  });
});
