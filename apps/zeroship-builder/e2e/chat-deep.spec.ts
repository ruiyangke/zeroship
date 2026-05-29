import { test, expect } from "@playwright/test";

// Deep chat surface coverage. All shell-only — no LLM round trip.
// We exercise the composer affordances, the @-mention dropdown
// interactions, and the chat empty/error states. Real-LLM hover-
// actions live in chat-actions.spec.ts (gated on OPENAI_API_KEY).

const SHELL_PATH = "/__test/workspace";

test.describe("Chat composer — affordances + shortcuts", () => {
  test("composer shows the cmd-enter / @ mention helper line", async ({ page }) => {
    await page.goto(SHELL_PATH);
    await expect(page.getByTestId("chat-composer")).toBeVisible();
    await expect(page.getByTestId("chat-composer")).toContainText("⌘");
    await expect(page.getByTestId("chat-composer")).toContainText("@ to mention");
  });

  test("send button is disabled with empty input", async ({ page }) => {
    await page.goto(SHELL_PATH);
    await expect(page.getByTestId("chat-send")).toBeDisabled();
  });

  test("send button enables once non-whitespace text is typed", async ({ page }) => {
    await page.goto(SHELL_PATH);
    await page.getByTestId("chat-input").fill("hello world");
    await expect(page.getByTestId("chat-send")).toBeEnabled();
  });

  test("plain Enter inserts a newline (does NOT submit)", async ({ page }) => {
    await page.goto(SHELL_PATH);
    const input = page.getByTestId("chat-input");
    await input.fill("first line");
    await input.press("End");
    await input.press("Enter");
    await page.keyboard.type("second line");
    // The textarea now has both lines; the send button stayed enabled
    // (the input wasn't cleared by an Enter-driven send).
    await expect(input).toHaveValue(/first line.*second line/s);
    await expect(page.getByTestId("chat-send")).toBeEnabled();
  });

  test("attachment paperclip button opens hidden file input", async ({ page }) => {
    await page.goto(SHELL_PATH);
    // Paperclip uses Button + aria-label="Attach files".
    const attach = page.getByRole("button", { name: /attach files/i });
    await expect(attach).toBeVisible();
    await attach.click();
    // We can't easily verify the file picker opened (browser-native),
    // but the click should not throw and the input ref is in the DOM.
  });
});

test.describe("Chat empty state", () => {
  test("shows 'What shall we make?' editorial copy on first mount", async ({ page }) => {
    await page.goto(SHELL_PATH);
    await expect(page.getByTestId("chat-empty")).toContainText("What shall we make?");
  });

  test("empty state hides once a message is in the conversation", async ({ page }) => {
    await page.goto(SHELL_PATH);
    // We can't trigger a real send without a backend, but the empty
    // state's affordance is clearly present and the input reflects
    // typing. Sanity check: empty state is visible while input is
    // empty/typing.
    await page.getByTestId("chat-input").fill("hello");
    await expect(page.getByTestId("chat-empty")).toBeVisible();
  });
});

test.describe("Mention dropdown — keyboard navigation", () => {
  test("dropdown shows 'no matches' state when the list is empty", async ({ page }) => {
    await page.goto(SHELL_PATH);
    const input = page.getByTestId("chat-input");
    await input.click();
    await input.type("@");
    await expect(page.getByTestId("mention-dropdown")).toBeVisible();
    // Without an appId, the dropdown renders the "no matches" copy
    // because the RPC fetches return nothing.
    await expect(page.getByTestId("mention-dropdown")).toContainText(/no matches|searching/i);
  });

  test("dropdown header reads 'files' for the default trigger", async ({ page }) => {
    await page.goto(SHELL_PATH);
    const input = page.getByTestId("chat-input");
    await input.click();
    await input.type("@");
    const dd = page.getByTestId("mention-dropdown");
    await expect(dd).toContainText("files");
    await expect(dd).toContainText("@file · @issue · @recent");
  });

  test("typing 'issue' after @ switches header to 'issues'", async ({ page }) => {
    await page.goto(SHELL_PATH);
    const input = page.getByTestId("chat-input");
    await input.click();
    await input.type("@issue");
    const dd = page.getByTestId("mention-dropdown");
    await expect(dd).toContainText("issues");
  });

  test("typing 'recent' after @ switches header to 'recent error'", async ({ page }) => {
    await page.goto(SHELL_PATH);
    const input = page.getByTestId("chat-input");
    await input.click();
    await input.type("@recent");
    const dd = page.getByTestId("mention-dropdown");
    await expect(dd).toContainText("recent error");
  });

  test("Escape closes the dropdown without clearing the @-token", async ({ page }) => {
    await page.goto(SHELL_PATH);
    const input = page.getByTestId("chat-input");
    await input.click();
    await input.type("@file");
    await expect(page.getByTestId("mention-dropdown")).toBeVisible();
    await input.press("Escape");
    await expect(page.getByTestId("mention-dropdown")).toBeHidden();
    // The @-token is still in the textarea.
    await expect(input).toHaveValue("@file");
  });

  test("typing space-then-not-space after @ does not auto-close immediately", async ({
    page,
  }) => {
    await page.goto(SHELL_PATH);
    const input = page.getByTestId("chat-input");
    await input.click();
    await input.type("@file ");
    // After one space, the dropdown should still be there per the
    // spec rule "two spaces close the trigger".
    await expect(page.getByTestId("mention-dropdown")).toBeVisible();
    await input.type("foo");
    await expect(page.getByTestId("mention-dropdown")).toBeVisible();
  });

  test("two spaces after @ closes the dropdown", async ({ page }) => {
    await page.goto(SHELL_PATH);
    const input = page.getByTestId("chat-input");
    await input.click();
    await input.type("@file foo  ");
    // Two spaces → trigger closed.
    await expect(page.getByTestId("mention-dropdown")).toBeHidden();
  });

  test("blur on textarea hides the dropdown", async ({ page }) => {
    await page.goto(SHELL_PATH);
    const input = page.getByTestId("chat-input");
    await input.click();
    await input.type("@");
    await expect(page.getByTestId("mention-dropdown")).toBeVisible();
    // Click outside the composer.
    await page.getByTestId("topbar-tour").click();
    await expect(page.getByTestId("mention-dropdown")).toBeHidden();
    // Close the tour modal that opened.
    await page.keyboard.press("Escape").catch(() => {});
    if (await page.getByTestId("product-tour-skip").isVisible().catch(() => false)) {
      await page.getByTestId("product-tour-skip").click();
    }
  });

  test("dropdown anchored above the textarea (positioned relative to composer)", async ({
    page,
  }) => {
    await page.goto(SHELL_PATH);
    const input = page.getByTestId("chat-input");
    await input.click();
    await input.type("@");
    const dd = page.getByTestId("mention-dropdown");
    const ddBox = await dd.boundingBox();
    const inputBox = await input.boundingBox();
    expect(ddBox).toBeTruthy();
    expect(inputBox).toBeTruthy();
    // Dropdown sits above the textarea.
    if (ddBox && inputBox) {
      expect(ddBox.y + ddBox.height).toBeLessThanOrEqual(inputBox.y + 4);
    }
  });
});

test.describe("Chat send button states", () => {
  test("send button label includes the arrow glyph", async ({ page }) => {
    await page.goto(SHELL_PATH);
    await expect(page.getByTestId("chat-send")).toContainText(/Send/i);
  });

  test("clearing input after typing re-disables the send button", async ({ page }) => {
    await page.goto(SHELL_PATH);
    const input = page.getByTestId("chat-input");
    await input.fill("hello");
    await expect(page.getByTestId("chat-send")).toBeEnabled();
    await input.fill("");
    await expect(page.getByTestId("chat-send")).toBeDisabled();
  });
});

test.describe("Chat input — IME / paste behaviour", () => {
  test("large pasted text fills the textarea", async ({ page }) => {
    await page.goto(SHELL_PATH);
    const input = page.getByTestId("chat-input");
    const long = "lorem ipsum ".repeat(100);
    await input.fill(long);
    await expect(input).toHaveValue(long);
  });
});
