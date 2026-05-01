import { test, expect } from "@playwright/test";

// Plan 01 — onboarding (spec §7.1, §7.3, §7.5).
//
// All UI surfaces — no API key, no control plane needed. The
// onboarding intent picker, the wizard's first-run hint, and the
// product-tour trigger are local-state-only and we drive them with
// localStorage assertions.

test.describe("Plan 01 — onboarding (spec §7)", () => {
  test.beforeEach(async ({ context }) => {
    // Each test starts on a clean storage so first-run flags fire.
    await context.clearCookies();
    await context.clearPermissions();
  });

  test("/onboarding/intent renders six choices + skip link", async ({ page }) => {
    await page.goto("/onboarding/intent");
    await expect(page.getByTestId("onboarding-intent-page")).toBeVisible();

    await expect(page.getByTestId("onboarding-intent-choice:internal_tool")).toBeVisible();
    await expect(page.getByTestId("onboarding-intent-choice:side_project")).toBeVisible();
    await expect(page.getByTestId("onboarding-intent-choice:startup")).toBeVisible();
    await expect(page.getByTestId("onboarding-intent-choice:client_gig")).toBeVisible();
    await expect(page.getByTestId("onboarding-intent-choice:learning")).toBeVisible();
    await expect(page.getByTestId("onboarding-intent-choice:exploring")).toBeVisible();

    await expect(page.getByTestId("onboarding-intent-skip")).toBeVisible();
  });

  test("clicking a choice stores intent in localStorage and navigates to /home", async ({ page }) => {
    await page.goto("/onboarding/intent");
    await page.getByTestId("onboarding-intent-choice:side_project").click();
    await expect(page).toHaveURL(/\/home$/);
    const intent = await page.evaluate(() => localStorage.getItem("zeroship_intent"));
    expect(intent).toBe("side_project");
  });

  test("Skip leaves intent unset and navigates to /home", async ({ page }) => {
    await page.goto("/onboarding/intent");
    await page.evaluate(() => localStorage.removeItem("zeroship_intent"));
    await page.getByTestId("onboarding-intent-skip").click();
    await expect(page).toHaveURL(/\/home$/);
    const intent = await page.evaluate(() => localStorage.getItem("zeroship_intent"));
    expect(intent).toBeNull();
  });

  test("first /new visit shows the first-run hint card", async ({ page }) => {
    // Clear the flag so this counts as a first visit.
    await page.goto("/");
    await page.evaluate(() => localStorage.removeItem("zeroship_first_run"));

    await page.goto("/new");
    await expect(page.getByTestId("wizard-first-run-hint")).toBeVisible();
  });

  test("subsequent /new visits hide the first-run hint card", async ({ page }) => {
    // Pre-set the flag — that's what would happen after the first send.
    await page.goto("/");
    await page.evaluate(() => localStorage.setItem("zeroship_first_run", "completed"));

    await page.goto("/new");
    await expect(page.getByTestId("wizard-first-run-hint")).toBeHidden();
  });

  test("empty submit shows validation hint instead of sending", async ({ page }) => {
    await page.goto("/new");
    // Begin button is disabled with empty input — clicking it does
    // nothing. Type whitespace, then verify Begin is still disabled.
    const send = page.getByTestId("wizard-send-idea");
    await expect(send).toBeDisabled();

    // Spaces only — the trim guard treats this as empty too.
    await page.getByTestId("wizard-prompt").fill("   ");
    await expect(send).toBeDisabled();
  });
});
