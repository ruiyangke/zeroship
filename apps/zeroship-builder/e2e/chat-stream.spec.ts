// ─── Chat streaming — user typed "hi", expects a streamed reply ──
//
// Reproduces the bug the human reported: typing "hi" in the chat
// produces no response. Asserts the *frontend* state, not just the
// backend side-effects: the assistant turn must mount, text content
// must arrive into it, and the chat status must return to idle.
//
// Skipped by default like the other AI test — needs a real LLM key
// in the dev runtime's env.

import { test, expect } from "@playwright/test";
import {
  createApp,
  deleteApp,
  uniqueAppName,
  visit,
  type AppRecord,
} from "./helpers";

const enabled = process.env.E2E_AI_BUILD === "1";

test.describe("chat streaming", () => {
  test.skip(!enabled, "set E2E_AI_BUILD=1 to enable real-LLM chat tests");

  test.setTimeout(60_000);
  test.use({ actionTimeout: 30_000, navigationTimeout: 30_000 });

  let app: AppRecord;

  test.beforeAll(async ({ request }) => {
    app = await createApp(request, uniqueAppName("e2e-chat"));
  });

  test.afterAll(async ({ request }) => {
    if (app) await deleteApp(request, app.id);
  });

  test('typing "hi" produces a visible streamed assistant reply', async ({ page }) => {
    page.on("console", (m) => {
      // eslint-disable-next-line no-console
      console.log(`[browser:${m.type()}]`, m.text());
    });
    page.on("pageerror", (e) => {
      // eslint-disable-next-line no-console
      console.log(`[pageerror]`, e.message);
    });
    page.on("requestfailed", (req) => {
      // eslint-disable-next-line no-console
      console.log(`[requestfailed]`, req.url(), req.failure()?.errorText);
    });

    await visit(page, `/p/${app.id}/preview`);
    await expect(page.getByTestId("chat-rail")).toBeVisible();

    // No turns yet
    await expect(page.getByTestId("turn-bot")).toHaveCount(0);

    await page.getByTestId("chat-input").fill("hi");
    await page.getByTestId("chat-send").click();

    // The user turn lands immediately
    const userTurn = page.getByTestId("turn-user");
    await expect(userTurn).toHaveCount(1);
    await expect(userTurn).toContainText("hi");

    // Within ~30s: an assistant turn mounts AND has text content
    const botTurn = page.getByTestId("turn-bot");
    await expect(botTurn).toHaveCount(1, { timeout: 30_000 });

    // Wait for the chat to fall back to idle, then inspect.
    await expect(page.getByTestId("status-title")).toHaveCount(0, { timeout: 30_000 });

    // Strip the "The studio" header — what's left is the streamed body.
    const text = (await botTurn.textContent()) ?? "";
    const body = text.replace(/The studio/i, "").replace(/\s+/g, " ").trim();
    expect(body.length, `assistant turn body: ${JSON.stringify(body)}`).toBeGreaterThan(2);

    // eslint-disable-next-line no-console
    console.log(`[chat-stream] assistant body: ${JSON.stringify(body.slice(0, 300))}`);
  });
});
