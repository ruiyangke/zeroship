import { test, expect } from "@playwright/test";

const HAS_KEY = !!process.env.OPENAI_API_KEY;

// Plan 03.1 made `/` render <Home> (project gallery). The workspace
// shell still mounts at /p/:appId/* (real project) and at the catch-
// all (no appId, default name=untitled). These tests target the
// catch-all path so they don't depend on the control plane being up.
const SHELL_PATH = "/__catchall_for_test";

test.describe("Plan 02 Phase A — workspace shell + real OpenAI Builder", () => {
  test("renders the shell with project name and chat rail", async ({ page }) => {
    await page.goto(SHELL_PATH);
    await expect(page.getByTestId("topbar")).toBeVisible();
    await expect(page.getByTestId("topbar-project")).toContainText("untitled");
    await expect(page.getByTestId("topbar-url")).toContainText(".zeroship.app");
    await expect(page.getByTestId("chat-rail")).toBeVisible();
    await expect(page.getByTestId("preview-canvas")).toContainText("Nothing's been built");
  });

  test("submits a prompt and streams a real Builder reply", async ({ page }) => {
    test.skip(!HAS_KEY, "OPENAI_API_KEY not set — skipping real-LLM test");

    await page.goto(SHELL_PATH);
    const input = page.getByTestId("chat-input");
    await input.fill("Reply with exactly: foo bar baz qux");
    await input.press("Control+Enter");

    await expect(page.getByTestId("msg-user")).toContainText("Reply with exactly");
    const assistant = page.getByTestId("msg-assistant").last();
    // LLM is non-deterministic — assert the bubble eventually carries
    // some non-empty assistant text. Tighter assertions belong on a fixture
    // model, not on the live API.
    await expect(assistant).not.toBeEmpty({ timeout: 30_000 });
  });

  test("stop button cancels an in-flight stream", async ({ page }) => {
    test.skip(!HAS_KEY, "OPENAI_API_KEY not set — skipping real-LLM test");

    await page.goto(SHELL_PATH);
    await page.getByTestId("chat-input").fill("Write a long essay about birds.");
    await page.getByTestId("chat-input").press("Control+Enter");
    await expect(page.getByTestId("chat-stop")).toBeVisible({ timeout: 10_000 });
    await page.getByTestId("chat-stop").click();
    await expect(page.getByTestId("chat-send")).toBeVisible({ timeout: 5_000 });
  });
});
