import { expect, test } from "@playwright/test";

// The assistant's wording is model-generated, so nothing here asserts on it.
// The claims are about the wire a user can actually see: the shell renders,
// the prompt is echoed, an assistant bubble fills in, and the form returns to
// idle only once the stream has closed.

test("renders the chat shell without contacting the provider", async ({ page }) => {
  await page.goto("/", { waitUntil: "domcontentloaded" });

  await expect(page.getByRole("heading", { name: "AI Chat" })).toBeVisible();
  await expect(page.getByPlaceholder(/Ask anything/)).toBeVisible();
  await expect(page.getByRole("button", { name: "Send" })).toBeVisible();
  await expect(page.getByText("Say hi to start a conversation.")).toBeVisible();
});

test("streams an assistant reply for a submitted prompt", async ({ page }) => {
  const failures: string[] = [];
  page.on("pageerror", (error) => failures.push(error.message));

  await page.goto("/", { waitUntil: "domcontentloaded" });
  await expect(page.getByRole("heading", { name: "AI Chat" })).toBeVisible();

  const prompt = "Reply in exactly five words.";
  await page.getByPlaceholder(/Ask anything/).fill(prompt);
  await page.getByRole("button", { name: "Send" }).click();

  // The prompt is echoed into the thread.
  await expect(page.locator("li", { hasText: prompt })).toBeVisible();

  // An assistant bubble appears once the first text delta lands. While the
  // stream is open the form shows Stop and the field is disabled; both flip
  // back together on the `[DONE]` frame, so waiting for Send to return is
  // what proves the stream closed rather than merely paused.
  const assistant = page
    .locator("li")
    .filter({ has: page.locator("div", { hasText: /^assistant$/ }) });
  await expect(assistant).toBeVisible({ timeout: 30_000 });
  await expect(page.getByPlaceholder(/Ask anything/)).toBeEnabled({ timeout: 60_000 });

  const reply = (await assistant.innerText()).replace(/^assistant\s*/i, "").trim();
  expect(reply.length).toBeGreaterThan(0);
  expect(failures).toEqual([]);
});
