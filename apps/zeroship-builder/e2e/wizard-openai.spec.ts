import { test, expect } from "@playwright/test";

const HAS_KEY = !!process.env.OPENAI_API_KEY;
// Smoke is heavy: end-to-end with real OpenAI structured-output. Each
// step (initial decide call, post-survey decide call) is a real model
// round-trip, so the wall budget is generous.
const STEP_TIMEOUT = 60_000;

test.describe("wizard surface (real OpenAI clarifier)", () => {
  test("renders the /new page even without an API key", async ({ page }) => {
    await page.goto("/new");
    await expect(page.getByTestId("wizard-prompt")).toBeVisible();
    await expect(page.getByTestId("wizard-send-idea")).toBeVisible();
  });

  test("idea → survey → answers → brief → Begin", async ({ page }) => {
    test.skip(!HAS_KEY, "OPENAI_API_KEY not set — skipping real-LLM test");

    await page.goto("/new");

    // 1. Type an idea and send. NotebookPrompt forwards data-testid
    // directly onto the <textarea>, so the testid selector IS the
    // input — no inner .locator chain needed.
    await page.getByTestId("wizard-prompt").fill(
      "A recipe app for my supper club where guests sign in, post photos, vote on who hosts next.",
    );
    await page.getByTestId("wizard-send-idea").click();

    // 2. Composer disappears (wizard takes over).
    await expect(page.getByTestId("wizard-prompt")).toBeHidden({ timeout: STEP_TIMEOUT });

    // 3. SurveyCard streams in.
    const survey = page.getByTestId("survey-card");
    await expect(survey).toBeVisible({ timeout: STEP_TIMEOUT });

    // 4. Answer required questions. The LLM is non-deterministic about
    // exact wording but the schema guarantees single_choice options
    // render as <Button>s inside the card. Click the first option for
    // each row.
    const questions = await survey.locator(".space-y-3 > div").all();
    expect(questions.length).toBeGreaterThan(0);
    for (const q of questions) {
      const firstOption = q.locator("button").first();
      if (await firstOption.isVisible()) {
        await firstOption.click();
      }
    }

    // 5. Submit.
    await survey.getByRole("button", { name: /send/i }).click();

    // 6. Card collapses to "Answered."
    await expect(page.getByTestId("survey-card-collapsed")).toBeVisible({ timeout: STEP_TIMEOUT });

    // 7. Brief arrives. The wizard may issue another survey first
    // (cap is 5); we accept either branch — wait for whichever happens.
    const brief = page.getByTestId("brief-card");
    const surveyAgain = page.getByTestId("survey-card");
    await Promise.race([
      brief.waitFor({ state: "visible", timeout: STEP_TIMEOUT }),
      surveyAgain.first().waitFor({ state: "visible", timeout: STEP_TIMEOUT }),
    ]);
    if (!(await brief.isVisible())) {
      // Second survey appeared — answer + submit identically; the LLM
      // should finalize after this round at latest given how concrete
      // the idea is.
      const more = page.getByTestId("survey-card").last();
      const moreQs = await more.locator(".space-y-3 > div").all();
      for (const q of moreQs) {
        const firstOption = q.locator("button").first();
        if (await firstOption.isVisible()) {
          await firstOption.click();
        }
      }
      await more.getByRole("button", { name: /send/i }).click();
      await expect(brief).toBeVisible({ timeout: STEP_TIMEOUT });
    }

    // 8. Brief carries a non-empty summary.
    await expect(brief).toContainText(/.{20,}/);

    // 9. Begin button is wired (we don't actually click it — the Plan
    // 01 stub fires window.alert which Playwright doesn't dismiss
    // without `page.on("dialog")` handling, and we just want to
    // confirm the button is present and enabled).
    await expect(page.getByTestId("brief-begin")).toBeEnabled();
  });
});
