import { test, expect } from "@playwright/test";

test.describe("Plan 01 — workspace shell + mock chat", () => {
  test("renders the shell with project name and chat rail", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByTestId("topbar")).toBeVisible();
    await expect(page.getByTestId("topbar-project")).toContainText("untitled");
    await expect(page.getByTestId("topbar-url")).toContainText(".zeroship.app");
    await expect(page.getByTestId("chat-rail")).toBeVisible();
    await expect(page.getByTestId("preview-canvas")).toContainText("Nothing's been built");
  });

  test("submits a prompt and streams a response with all data parts", async ({ page }) => {
    await page.goto("/");

    const input = page.getByTestId("chat-input");
    await input.fill("Build a recipe app for my supper club");

    // ⌘+Enter (Control+Enter cross-platform)
    await input.press("Control+Enter");

    // User turn shows up immediately
    await expect(page.getByTestId("msg-user")).toContainText("recipe app for my supper club");

    // Assistant turn streams in
    const assistant = page.getByTestId("msg-assistant").last();
    await expect(assistant).toContainText("Got it", { timeout: 5000 });

    // SurveyCard appears
    await expect(page.getByTestId("survey-card")).toBeVisible({ timeout: 5000 });

    // Receipt appears (the write_file tool call)
    await expect(page.getByTestId("receipt").first()).toBeVisible({ timeout: 10000 });
    await expect(page.getByTestId("receipt").first()).toContainText("Wrote");

    // DiffCard appears
    await expect(page.getByTestId("diff-card")).toBeVisible({ timeout: 10000 });
    await expect(page.getByTestId("diff-card")).toContainText("src/index.tsx");

    // CriticRoundCard appears
    await expect(page.getByTestId("critic-round-card")).toBeVisible({ timeout: 10000 });
    await expect(page.getByTestId("critic-round-card")).toContainText("approved");
  });

  test("stop button cancels an in-flight stream", async ({ page }) => {
    await page.goto("/");
    await page.getByTestId("chat-input").fill("hello");
    await page.getByTestId("chat-input").press("Control+Enter");
    await expect(page.getByTestId("chat-stop")).toBeVisible({ timeout: 5000 });
    await page.getByTestId("chat-stop").click();
    await expect(page.getByTestId("chat-send")).toBeVisible({ timeout: 5000 });
  });

  test("survey 'skip' collapses the card", async ({ page }) => {
    await page.goto("/");
    await page.getByTestId("chat-input").fill("test");
    await page.getByTestId("chat-input").press("Control+Enter");
    await expect(page.getByTestId("survey-card")).toBeVisible({ timeout: 8000 });
    await page.getByText("skip — just build").click();
    await expect(page.getByTestId("survey-card-collapsed")).toBeVisible();
  });
});
