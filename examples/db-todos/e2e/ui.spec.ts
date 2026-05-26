import { expect, test, type BrowserContext, type Page } from "@playwright/test";

const uniq = (prefix: string) =>
  `${prefix}-${Date.now().toString(36)}${Math.random().toString(36).slice(2, 5)}`;

async function openCount(page: Page): Promise<number> {
  const txt = await page.locator(".count").innerText();
  const m = txt.match(/(\d+)/);
  return m ? Number(m[1]) : NaN;
}

async function bootedPage(page: Page): Promise<Page> {
  page.on("pageerror", (err) => console.error(`[browser pageerror] ${err.message}`));
  await page.goto("/", { waitUntil: "domcontentloaded", timeout: 30_000 });
  await expect(page.getByRole("heading", { name: "Todos" })).toBeVisible({ timeout: 15_000 });
  await expect(page.getByPlaceholder("Add a task…")).toBeEnabled({ timeout: 45_000 });
  return page;
}

async function addTodo(page: Page, { priority = "high" }: { priority?: "low" | "medium" | "high" } = {}) {
  const title = uniq("e2e");
  await page.getByPlaceholder("Add a task…").fill(title);
  await page.locator(`.prio button[aria-label="${priority} priority"]`).click();
  await page.locator("button.add").click();
  await expect(page.locator(".item", { hasText: title }).first()).toBeVisible({ timeout: 10_000 });
  return title;
}

test.describe.serial("db-todos UI", () => {
  let context: BrowserContext;
  let page: Page;
  let createdTitle: string;

  test.beforeAll(async ({ browser }) => {
    context = await browser.newContext();
    page = await bootedPage(await context.newPage());
  });

  test.afterAll(async () => {
    await context?.close();
  });

  test("app mounts with heading and enabled composer", async () => {
    await expect(page.getByRole("heading", { name: "Todos" })).toBeVisible();
    await expect(page.getByPlaceholder("Add a task…")).toBeEnabled();
  });

  test("LIVE indicator turns on after SSE connects", async () => {
    await expect(page.locator(".live.on")).toBeVisible({ timeout: 15_000 });
  });

  test("create adds a composer todo that renders", async () => {
    createdTitle = await addTodo(page, { priority: "high" });
    await expect(page.locator(".item", { hasText: createdTitle }).first()).toBeVisible();
  });

  test("toggle done marks the created todo done", async () => {
    const item = page.locator(".item", { hasText: createdTitle }).first();
    await item.locator(".box").click();
    await expect(page.locator(".item.done", { hasText: createdTitle }).first()).toBeVisible({ timeout: 10_000 });
  });

  test("toggle back unmarks the created todo", async () => {
    const item = page.locator(".item", { hasText: createdTitle }).first();
    await item.locator(".box").click();
    await expect(page.locator(".item.done", { hasText: createdTitle })).toHaveCount(0, { timeout: 10_000 });
  });

  test("archive removes the created row from the list", async () => {
    const item = page.locator(".item", { hasText: createdTitle });
    await item.first().hover();
    await item.first().locator('.icon-btn[aria-label="archive"]').click();
    await expect(item).toHaveCount(0, { timeout: 10_000 });
  });

  test("delete soft-deletes and removes the row", async () => {
    const title = await addTodo(page);
    const item = page.locator(".item", { hasText: title });
    await item.first().hover();
    await item.first().locator('.icon-btn[aria-label="delete"]').click();
    await expect(item).toHaveCount(0, { timeout: 10_000 });
  });

  test("demo seeder bulk-creates ten open todos", async () => {
    const before = await openCount(page);
    await page.locator("button.ghost").click();
    await expect
      .poll(() => openCount(page), { timeout: 20_000 })
      .toBeGreaterThanOrEqual(before + 10);
  });

  test("realtime sends a todo created in tab A to tab B", async ({ browser }) => {
    const contextB = await browser.newContext();
    const pageB = await bootedPage(await contextB.newPage());
    try {
      const title = uniq("rt");
      await page.getByPlaceholder("Add a task…").fill(title);
      await page.locator("button.add").click();
      await expect(pageB.locator(".item", { hasText: title }).first()).toBeVisible({ timeout: 15_000 });
    } finally {
      await contextB.close();
    }
  });
});
