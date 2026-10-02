import { expect, test, type APIRequestContext } from "@playwright/test";
import {
  ERROR_MESSAGE,
  ERROR_PROMPT,
  MODEL,
  STREAM_PROMPT,
  STREAM_REPLY,
  STUB_API_KEY,
  STUB_ORIGIN,
  messageText,
} from "../tests/fixture/settings.ts";

// The provider is the local stub from `tests/fixture`, so every reply is
// deterministic. Each test also inspects what the stub received: a passing
// suite means the app really called the configured provider with the model,
// messages and key it was given.
//
// The claims are about the wire a user can actually see: the shell renders,
// the prompt is echoed, the assistant bubble fills in with the provider's
// streamed deltas, and the form returns to idle only once the stream closed.

interface CapturedRequest {
  path: string;
  model: string;
  messages: Array<{ role: string; content: unknown }>;
  authorization: string | null;
}

async function captured(request: APIRequestContext): Promise<CapturedRequest[]> {
  const response = await request.get(`${STUB_ORIGIN}/__requests`);
  expect(response.ok()).toBeTruthy();
  return (await response.json()) as CapturedRequest[];
}

test.beforeEach(async ({ request }) => {
  const response = await request.post(`${STUB_ORIGIN}/__reset`);
  expect(response.ok()).toBeTruthy();
});

test("renders the chat shell without contacting the provider", async ({ page, request }) => {
  await page.goto("/", { waitUntil: "domcontentloaded" });

  await expect(page.getByRole("heading", { name: "AI Chat" })).toBeVisible();
  await expect(page.getByPlaceholder(/Ask anything/)).toBeVisible();
  await expect(page.getByRole("button", { name: "Send" })).toBeVisible();
  await expect(page.getByText("Say hi to start a conversation.")).toBeVisible();

  expect(await captured(request)).toEqual([]);
});

test("streams an assistant reply assembled from the provider's chunks", async ({ page, request }) => {
  const failures: string[] = [];
  page.on("pageerror", (error) => failures.push(error.message));

  await page.goto("/", { waitUntil: "domcontentloaded" });
  await expect(page.getByRole("heading", { name: "AI Chat" })).toBeVisible();

  await page.getByPlaceholder(/Ask anything/).fill(STREAM_PROMPT);
  await page.getByRole("button", { name: "Send" }).click();

  // The prompt is echoed into the thread.
  await expect(page.locator("li", { hasText: STREAM_PROMPT })).toBeVisible();

  // An assistant bubble appears once the first text delta lands. While the
  // stream is open the form shows Stop and the field is disabled; both flip
  // back together on the `[DONE]` frame, so waiting for Send to return is
  // what proves the stream closed rather than merely paused.
  const assistant = page
    .locator("li")
    .filter({ has: page.locator("div", { hasText: /^assistant$/ }) });
  await expect(assistant).toBeVisible({ timeout: 30_000 });
  await expect(page.getByPlaceholder(/Ask anything/)).toBeEnabled({ timeout: 60_000 });

  // Every delta the stub sent is present, in order, in one bubble.
  const reply = (await assistant.innerText()).replace(/^assistant\s*/i, "").trim();
  expect(reply).toBe(STREAM_REPLY);

  const requests = await captured(request);
  expect(requests).toHaveLength(1);
  expect(requests[0].path).toBe("/v1/chat/completions");
  expect(requests[0].model).toBe(MODEL);
  expect(requests[0].authorization).toBe(`Bearer ${STUB_API_KEY}`);
  expect(
    requests[0].messages.filter((m) => m.role === "user").map((m) => messageText(m.content)),
  ).toEqual([STREAM_PROMPT]);
  expect(failures).toEqual([]);
});

test("surfaces a provider failure to the chat UI", async ({ page, request }) => {
  await page.goto("/", { waitUntil: "domcontentloaded" });
  await expect(page.getByRole("heading", { name: "AI Chat" })).toBeVisible();

  await page.getByPlaceholder(/Ask anything/).fill(ERROR_PROMPT);
  await page.getByRole("button", { name: "Send" }).click();

  const error = page.getByText(/error:/i);
  await expect(error).toBeVisible({ timeout: 30_000 });
  await expect(error).toContainText(ERROR_MESSAGE);
  await expect(page.getByPlaceholder(/Ask anything/)).toBeEnabled({ timeout: 60_000 });

  const requests = await captured(request);
  expect(requests.length).toBeGreaterThan(0);
  expect(requests.every((r) => r.model === MODEL)).toBe(true);
  expect(
    requests.some((r) =>
      r.messages.some((m) => m.role === "user" && messageText(m.content) === ERROR_PROMPT),
    ),
  ).toBe(true);
});
