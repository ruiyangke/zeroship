import { test, expect } from "@playwright/test";

test.describe("Plan 01.5 — workspace shell + v6 mock chat", () => {
  test("renders the shell with project name and chat rail", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByTestId("topbar")).toBeVisible();
    await expect(page.getByTestId("topbar-project")).toContainText("untitled");
    await expect(page.getByTestId("topbar-url")).toContainText(".zeroship.app");
    await expect(page.getByTestId("chat-rail")).toBeVisible();
    await expect(page.getByTestId("preview-canvas")).toContainText("Nothing's been built");
  });

  test("submits a prompt and streams a text response", async ({ page }) => {
    await page.goto("/");

    const input = page.getByTestId("chat-input");
    await input.fill("Build a recipe app");
    await input.press("Control+Enter");

    await expect(page.getByTestId("msg-user")).toContainText("Build a recipe app");

    const assistant = page.getByTestId("msg-assistant").last();
    await expect(assistant).toContainText("Plan 01.5", { timeout: 15_000 });
  });

  test("stop button cancels an in-flight stream", async ({ page }) => {
    await page.goto("/");
    await page.getByTestId("chat-input").fill("hello");
    await page.getByTestId("chat-input").press("Control+Enter");
    await expect(page.getByTestId("chat-stop")).toBeVisible({ timeout: 5_000 });
    await page.getByTestId("chat-stop").click();
    await expect(page.getByTestId("chat-send")).toBeVisible({ timeout: 5_000 });
  });
});
