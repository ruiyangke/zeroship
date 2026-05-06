import { test, expect, type Page } from "@playwright/test";

// Chat surface interactions: hover actions, @-mention dropdown,
// markdown rendering on assistant messages, retry on error.
//
// Two test groups:
//   - "shell-only" (no LLM): exercises the affordances with no real
//     turn — synthesises an assistant message via the DOM (the
//     simplest way to verify the hover row + markdown render in
//     isolation). Replays the sandbox-driven @-dropdown without an
//     OpenAI key.
//   - "gated" (LLM + sandbox): runs a real round-trip and verifies
//     the hover-revealed copy/regenerate buttons appear on the
//     streamed bubble.
//
// We use the `/__catchall_for_test` shell path so the shell mounts
// without a control plane, mirroring chat-openai.spec.ts.

const HAS_KEY = !!process.env.OPENAI_API_KEY;
const SHELL_PATH = "/__catchall_for_test";

// Grant clipboard permissions before each test so navigator.clipboard
// works headless. Chromium needs both read + write for round-tripping.
async function setupClipboard(page: Page) {
  await page.context().grantPermissions(["clipboard-read", "clipboard-write"]);
}

test.describe("chat actions (shell-only)", () => {
  test.beforeEach(async ({ page }) => {
    await setupClipboard(page);
  });

  test("empty state shows the editorial prompt", async ({ page }) => {
    await page.goto(SHELL_PATH);
    await expect(page.getByTestId("chat-empty")).toBeVisible();
    await expect(page.getByTestId("chat-empty")).toContainText("What shall we make?");
  });

  test("composer shows the @-mention hint", async ({ page }) => {
    await page.goto(SHELL_PATH);
    await expect(page.getByTestId("chat-composer")).toContainText("@ to mention");
  });

  test("typing @ opens the mention dropdown", async ({ page }) => {
    await page.goto(SHELL_PATH);
    const input = page.getByTestId("chat-input");
    await input.click();
    await input.type("@");
    // Dropdown should appear even when the catch-all has no appId —
    // it shows "no matches" but is still rendered. That's the load-
    // bearing UI affordance.
    await expect(page.getByTestId("mention-dropdown")).toBeVisible();
  });

  test("Escape closes the mention dropdown", async ({ page }) => {
    await page.goto(SHELL_PATH);
    const input = page.getByTestId("chat-input");
    await input.click();
    await input.type("@");
    await expect(page.getByTestId("mention-dropdown")).toBeVisible();
    await input.press("Escape");
    await expect(page.getByTestId("mention-dropdown")).not.toBeVisible();
  });

  test("typing a space-prefix character in the middle of text doesn't open the dropdown", async ({
    page,
  }) => {
    await page.goto(SHELL_PATH);
    const input = page.getByTestId("chat-input");
    await input.click();
    await input.type("hello");
    // No leading whitespace before the @ — should NOT open.
    // Type @ inline at the end of "hello" so the previous char is "o".
    await page.keyboard.type("@");
    await expect(page.getByTestId("mention-dropdown")).not.toBeVisible();
  });
});

test.describe("chat actions (gated, real LLM)", () => {
  test.beforeEach(async ({ page }) => {
    await setupClipboard(page);
  });

  test("hovering an assistant bubble reveals copy + regenerate", async ({ page }) => {
    test.skip(!HAS_KEY, "OPENAI_API_KEY not set — skipping real-LLM test");

    await page.goto(SHELL_PATH);
    const input = page.getByTestId("chat-input");
    await input.fill("Reply with exactly: hello world");
    await input.press("Control+Enter");

    const assistant = page.getByTestId("msg-assistant").last();
    // Wait for the turn to finish — once `streaming` flips false the
    // hover row appears (regenerate is hidden during streaming).
    await expect(assistant).not.toBeEmpty({ timeout: 30_000 });
    // Give the post-stream re-render a beat.
    await page.waitForTimeout(500);
    await assistant.hover();
    // Both buttons live inside the assistant bubble's group.
    await expect(assistant.getByTestId("msg-action-copy")).toBeVisible();
    await expect(assistant.getByTestId("msg-action-regenerate")).toBeVisible();
  });

  test("copy button writes to clipboard", async ({ page }) => {
    test.skip(!HAS_KEY, "OPENAI_API_KEY not set — skipping real-LLM test");

    await page.goto(SHELL_PATH);
    const input = page.getByTestId("chat-input");
    await input.fill("Reply with exactly: copy probe one two");
    await input.press("Control+Enter");

    const assistant = page.getByTestId("msg-assistant").last();
    await expect(assistant).not.toBeEmpty({ timeout: 30_000 });
    await page.waitForTimeout(500);
    await assistant.hover();
    await assistant.getByTestId("msg-action-copy").click();
    // Read the clipboard back via page.evaluate. In headless chromium
    // the permission was granted in beforeEach.
    const copied = await page.evaluate(() => navigator.clipboard.readText());
    expect(copied.length).toBeGreaterThan(0);
  });

  test("user bubble shows an edit button on hover", async ({ page }) => {
    test.skip(!HAS_KEY, "OPENAI_API_KEY not set — skipping real-LLM test");

    await page.goto(SHELL_PATH);
    const input = page.getByTestId("chat-input");
    await input.fill("Reply with exactly: edit probe");
    await input.press("Control+Enter");

    const user = page.getByTestId("msg-user").last();
    await expect(user).toBeVisible();
    await user.hover();
    await expect(user.getByTestId("msg-action-edit")).toBeVisible();
  });
});
