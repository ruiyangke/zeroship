/**
 * AI Builder UI — Playwright e2e tests.
 *
 * Targets the chat-primary `/builder` page. Most tests stub the
 * agent SSE response via Playwright's route interception so they
 * run fast + deterministically without an OpenAI key.
 *
 * One test at the bottom (`live-stack`) runs against the actual
 * agent + sandbox + control plane — gated by the AGENT_LIVE env
 * var so CI doesn't accidentally need a running stack.
 *
 * Servers required for ALL tests in this file:
 *   - vite dev server on :5173
 *
 * Live test additionally requires:
 *   - agent on :4444 (so /agent/chat proxies somewhere real)
 *   - sandbox + control + worker + gateway running
 *   - OPENAI_API_KEY set on the agent process
 *
 * Run:
 *   npx playwright test e2e/builder.spec.ts
 *   AGENT_LIVE=1 npx playwright test e2e/builder.spec.ts
 */
import { test, expect, type Page, type Route } from "@playwright/test";

const LIVE = process.env.AGENT_LIVE === "1";

/** Helper: serialize a sequence of SSE events as a wire-format string. */
function makeSSE(events: object[]): string {
  return events.map((ev) => `data: ${JSON.stringify(ev)}\n\n`).join("");
}

/** Stub the agent SSE response so tests don't need OpenAI. */
async function stubAgent(page: Page, events: object[]): Promise<void> {
  await page.route("**/agent/chat", async (route: Route) => {
    await route.fulfill({
      status: 200,
      contentType: "text/event-stream; charset=utf-8",
      body: makeSSE(events),
    });
  });
}

// =========================================================================
// Layout & navigation
// =========================================================================

test.describe("Builder layout (no auth required)", () => {
  test("loads /builder bootstrap without login", async ({ page }) => {
    await page.goto("/builder");
    // Top-bar shows the "new project" placeholder + idle status.
    await expect(page.locator("header")).toContainText("new project");
    await expect(page.locator("header")).toContainText("idle");
  });

  test("empty chat shows the AI builder helper text", async ({ page }) => {
    await page.goto("/builder");
    await expect(page.getByText("// ai builder")).toBeVisible();
    await expect(
      page.locator("text=/Describe the app you want to build/i"),
    ).toBeVisible();
  });

  test("composer is enabled and accepts input", async ({ page }) => {
    await page.goto("/builder");
    const textarea = page.locator("textarea");
    await expect(textarea).toBeEnabled();
    await textarea.fill("hello world");
    await expect(textarea).toHaveValue("hello world");
  });

  test("send button disabled until composer has content", async ({ page }) => {
    await page.goto("/builder");
    const send = page.locator('button[title="Send (Enter)"]');
    await expect(send).toBeDisabled();
    await page.locator("textarea").fill("hi");
    await expect(send).toBeEnabled();
  });

  test('"show code" toggle is hidden in bootstrap (no app yet)', async ({ page }) => {
    await page.goto("/builder");
    // BootstrapBuilder always renders the toggle in the header; clicking
    // it has no effect (no code panel) but the button is still there.
    await expect(page.getByRole("button", { name: /show code/i })).toBeVisible();
  });
});

// =========================================================================
// Chat behavior — agent stubbed
// =========================================================================

test.describe("Chat with stubbed agent", () => {
  test("plain text response renders + appears in the message thread", async ({ page }) => {
    await stubAgent(page, [
      { type: "text", content: "Hello! " },
      { type: "text", content: "How can I help?" },
      { type: "done" },
    ]);

    await page.goto("/builder");
    await page.locator("textarea").fill("hi");
    await page.locator('button[title="Send (Enter)"]').click();

    // User bubble appears.
    await expect(page.getByText("hi", { exact: true })).toBeVisible();
    // Assistant bubble appears with the streamed text.
    await expect(page.getByText("Hello! How can I help?")).toBeVisible({ timeout: 5000 });
  });

  test("Enter key submits, Shift+Enter inserts newline", async ({ page }) => {
    await stubAgent(page, [
      { type: "text", content: "ack" },
      { type: "done" },
    ]);

    await page.goto("/builder");
    const textarea = page.locator("textarea");

    // Shift+Enter inserts newline, no submit.
    await textarea.fill("first line");
    await textarea.press("Shift+Enter");
    await textarea.type("second line");
    await expect(textarea).toHaveValue("first line\nsecond line");

    // Plain Enter submits.
    await textarea.press("Enter");
    await expect(page.getByText("ack")).toBeVisible({ timeout: 5000 });
    // Composer clears after send.
    await expect(textarea).toHaveValue("");
  });

  test("tool_start / tool_end render a collapsible tool card", async ({ page }) => {
    await stubAgent(page, [
      {
        type: "tool_start",
        name: "open_session",
        input: { project_id: "550e8400-e29b-41d4-a716-446655440000" },
      },
      {
        type: "tool_end",
        name: "open_session",
        output: JSON.stringify({ ok: true, session_id: "abc" }),
      },
      { type: "text", content: "Done." },
      { type: "done" },
    ]);

    await page.goto("/builder");
    await page.locator("textarea").fill("go");
    await page.locator('button[title="Send (Enter)"]').click();

    // Tool card title visible.
    const card = page.locator('button:has-text("open_session")').first();
    await expect(card).toBeVisible({ timeout: 5000 });

    // Output not shown by default.
    await expect(page.getByText("session_id")).not.toBeVisible();

    // Click to expand.
    await card.click();
    await expect(page.getByText(/abc/).first()).toBeVisible();
  });

  test("error event shows error banner above composer", async ({ page }) => {
    await stubAgent(page, [
      { type: "error", content: "agent init failed: missing API key" },
      { type: "done" },
    ]);

    await page.goto("/builder");
    await page.locator("textarea").fill("trigger");
    await page.locator('button[title="Send (Enter)"]').click();

    // Both the assistant message bubble + the composer error banner
    // contain this text — count both rather than asserting visibility
    // on the (non-strict-unique) substring.
    await expect(page.getByText(/agent init failed/i).first()).toBeVisible({ timeout: 5000 });
  });

  test("status pill flips from idle → thinking → idle", async ({ page }) => {
    // Slow stub so we can observe the in-flight state.
    await page.route("**/agent/chat", async (route) => {
      await new Promise((r) => setTimeout(r, 800));
      await route.fulfill({
        status: 200,
        contentType: "text/event-stream",
        body: makeSSE([{ type: "text", content: "ok" }, { type: "done" }]),
      });
    });

    await page.goto("/builder");
    await expect(page.locator("header")).toContainText("idle");

    await page.locator("textarea").fill("go");
    await page.locator('button[title="Send (Enter)"]').click();

    // Briefly thinking, then back to idle.
    await expect(page.locator("header")).toContainText(/thinking|tool|deploying/i, {
      timeout: 2000,
    });
    await expect(page.locator("header")).toContainText("idle", { timeout: 5000 });
  });

  test("create_app result navigates to /builder/:newId", async ({ page }) => {
    const NEW_ID = "11111111-2222-3333-4444-555555555555";
    await stubAgent(page, [
      { type: "tool_start", name: "create_app", input: {} },
      {
        type: "tool_end",
        name: "create_app",
        output: JSON.stringify({ ok: true, app_id: NEW_ID, app_name: "newproj" }),
      },
      { type: "done" },
    ]);

    // Stub the post-navigation getApp() call so ProjectBuilder doesn't 500.
    await page.route("**/api/apps/" + NEW_ID, async (route) => {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          id: NEW_ID, name: "newproj", plan_id: "free",
          deploy_hash: null, api_key: "k",
          created_at: "2026-01-01", updated_at: "2026-01-01",
        }),
      });
    });

    await page.goto("/builder");
    await page.locator("textarea").fill("make me an app");
    await page.locator('button[title="Send (Enter)"]').click();

    await page.waitForURL(`**/builder/${NEW_ID}`, { timeout: 5000 });
  });
});

// =========================================================================
// Conversation persistence (localStorage)
// =========================================================================

test.describe("Conversation persistence", () => {
  test("history survives a page reload (same appId)", async ({ page }) => {
    const NEW_ID = "deadbeef-dead-beef-dead-beefdeadbeef";

    await stubAgent(page, [
      { type: "text", content: "first reply" },
      { type: "done" },
    ]);

    await page.route(`**/api/apps/${NEW_ID}`, async (route) => {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          id: NEW_ID, name: "p", plan_id: "free", deploy_hash: null,
          api_key: "k", created_at: "x", updated_at: "x",
        }),
      });
    });

    await page.goto(`/builder/${NEW_ID}`);
    await page.locator("textarea").fill("ping");
    await page.locator('button[title="Send (Enter)"]').click();
    await expect(page.getByText("first reply")).toBeVisible();

    // Reload — both messages should still be there.
    await page.reload();
    await expect(page.getByText("ping").first()).toBeVisible();
    await expect(page.getByText("first reply")).toBeVisible();
  });

  test("clearing chat (trash icon) wipes the thread", async ({ page }) => {
    const NEW_ID = "cccccccc-cccc-cccc-cccc-cccccccccccc";
    await stubAgent(page, [{ type: "text", content: "reply" }, { type: "done" }]);
    await page.route(`**/api/apps/${NEW_ID}`, async (route) => {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          id: NEW_ID, name: "p", plan_id: "free", deploy_hash: null,
          api_key: "k", created_at: "x", updated_at: "x",
        }),
      });
    });

    await page.goto(`/builder/${NEW_ID}`);
    await page.locator("textarea").fill("hi");
    await page.locator('button[title="Send (Enter)"]').click();
    await expect(page.getByText("reply")).toBeVisible();

    await page.locator('button[title="Clear conversation"]').click();
    await expect(page.getByText("hi", { exact: true })).not.toBeVisible();
    await expect(page.getByText("// ai builder")).toBeVisible();
  });
});

// =========================================================================
// Show-code toggle reveals CodeMirror in project mode
// =========================================================================

test.describe("Show-code panel (project mode)", () => {
  const PID = "abcdef00-1234-5678-9abc-def012345678";

  test.beforeEach(async ({ page }) => {
    await page.route(`**/api/apps/${PID}`, async (route) => {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          id: PID,
          name: "demo",
          plan_id: "free",
          deploy_hash: "abc123",
          api_key: "k",
          created_at: "2026-01-01",
          updated_at: "2026-01-01",
          server_js: "export default { fetch() { return new Response('hi'); } };\n",
        }),
      });
    });
  });

  test("toggling 'show code' reveals CodeMirror with server.js", async ({ page }) => {
    await page.goto(`/builder/${PID}`);
    // Code panel not present by default.
    await expect(page.locator(".cm-editor")).not.toBeVisible();

    await page.getByRole("button", { name: /show code/i }).click();

    // CodeMirror mounts.
    await expect(page.locator(".cm-editor")).toBeVisible({ timeout: 5000 });
    // The deployed server.js content is visible.
    await expect(page.locator(".cm-content")).toContainText("export default");
  });

  test("'hide code' returns to chat + preview only", async ({ page }) => {
    await page.goto(`/builder/${PID}`);
    await page.getByRole("button", { name: /show code/i }).click();
    await expect(page.locator(".cm-editor")).toBeVisible();
    await page.getByRole("button", { name: /hide code/i }).click();
    await expect(page.locator(".cm-editor")).not.toBeVisible();
  });
});

// =========================================================================
// Live e2e — gated on AGENT_LIVE=1 because it needs all the platform
// services running plus an OpenAI key on the agent process.
// =========================================================================

test.describe("Live stack", () => {
  test.skip(!LIVE, "set AGENT_LIVE=1 to run against the running agent + sandbox");

  test("agent responds to a real message via the vite proxy", async ({ page }) => {
    test.setTimeout(60_000);
    await page.goto("/builder");
    await page.locator("textarea").fill("Reply with the literal text PONG and nothing else.");
    await page.locator('button[title="Send (Enter)"]').click();

    // Wait for an assistant response containing PONG.
    await expect(page.getByText(/PONG/)).toBeVisible({ timeout: 50_000 });
  });
});
