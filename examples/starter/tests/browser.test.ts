import { expect as playwrightExpect } from "@playwright/test";
import { launchChromium, pageFixtures, type PageWatch } from "@zeroship/example-fixtures";
import type { Browser, Page } from "playwright";
import { afterAll, beforeAll, describe, inject, test as base } from "vitest";
import { targets, type Target } from "./targets";

const expect = playwrightExpect.configure({ timeout: 15_000 });

/** The messages src/server.ts seeds. */
const SEEDED = ["Build locally with an AI coding agent.", "Run pnpm build to produce dist/app.zship."];

/** A Vitest `test` whose page is a fresh, watched page over `where`. */
function browserTest(where: Target) {
  let browser: Browser;
  beforeAll(async () => { browser = await launchChromium(); });
  afterAll(async () => { await browser?.close(); });
  return base.extend<{ page: Page; watch: PageWatch }>(pageFixtures(() => browser, where, inject("starterArtifacts")));
}

for (const where of targets()) describe(`starter on ${where.name}`, () => {
  const test = browserTest(where);
  const messages = (page: Page) => page.locator("ul li span");
  const unique = (prefix: string) => `${prefix} ${Date.now().toString(36)}${Math.random().toString(36).slice(2, 6)}`;

  async function add(page: Page, text: string) {
    await page.getByPlaceholder("Write a message").fill(text);
    await page.getByRole("button", { name: "Add" }).click();
    await expect(messages(page).filter({ hasText: text })).toHaveCount(1);
  }

  test("the app mounts and lists the messages the server seeds, read over getMessages", async ({ page }) => {
    await page.goto(where.uiUrl);
    await expect(page.locator("h1")).toHaveText("zeroship starter");
    await expect(messages(page).first()).toBeVisible();
    expect((await messages(page).allTextContents()).slice(0, SEEDED.length)).toEqual(SEEDED);
  });

  test("a message added through the form comes back in the list the server returns", async ({ page }) => {
    await page.goto(where.uiUrl);
    await expect(messages(page).first()).toBeVisible();
    const text = unique("added");
    await add(page, text);
    await expect(page.getByPlaceholder("Write a message")).toHaveValue("");
  });

  test("an added message is still listed after a full reload, because the server holds it", async ({ page }) => {
    await page.goto(where.uiUrl);
    await expect(messages(page).first()).toBeVisible();
    const text = unique("kept");
    await add(page, text);
    await page.reload();
    await expect(messages(page).filter({ hasText: text })).toHaveCount(1);
  });

  // The control for every other test here: the page fixture fails a test that
  // leaves an RPC answer of 400 or more behind, and reports the server's own
  // error from the target's log. `boom` throws inside its handler on purpose.
  test("a handler that throws is a problem the watch takes, with the runtime's own error from the server log", async ({ page, watch }) => {
    await page.goto(where.uiUrl);
    await expect(messages(page).first()).toBeVisible();
    const status = await page.evaluate(async () => {
      const response = await fetch("/__zeroship/v1/boom", {
        method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ json: {} }),
      });
      return response.status;
    });
    expect(status).toBe(500);
    expect(await watch.take()).toEqual([expect.stringContaining("/__zeroship/v1/boom answered 500")]);
    await expect.poll(() => watch.serverErrors()).toContainEqual(expect.stringContaining("[starter] boom: deliberate handler failure"));
  });
});
