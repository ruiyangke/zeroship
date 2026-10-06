import { expect as playwrightExpect } from "@playwright/test";
import { launchChromium, pageFixtures, type PageWatch } from "@zeroship/example-testkit";
import type { Browser, Page } from "playwright";
import { afterAll, beforeAll, describe, inject, test as base } from "vitest";
import { target, targets, type Target } from "./targets";

const expect = playwrightExpect.configure({ timeout: 15_000 });

/** Titles from src/components/posts.ts. */
const FIRST = "Why SSR is back in fashion";
const MIDDLE = "SSG vs SSR \u2014 pick on traffic shape";

/** A Vitest `test` whose page is a fresh, watched page over `where`. */
function browserTest(where: Target) {
  let browser: Browser;
  beforeAll(async () => { browser = await launchChromium(); });
  afterAll(async () => { await browser?.close(); });
  return base.extend<{ page: Page; watch: PageWatch }>(pageFixtures(() => browser, where, inject("ssrArtifacts")));
}

// React reports a hydration mismatch through `reportError`, which the browser
// raises as an uncaught error, so the page fixture fails any test here whose
// page did not hydrate cleanly.
for (const where of targets()) describe(`ssr-blog on ${where.name}`, () => {
  const test = browserTest(where);

  test("the server renders the post list into the document it sends, and its links lead to each post", async ({ page }) => {
    const response = await page.goto(where.uiUrl);
    expect(response?.status()).toBe(200);
    const html = await response!.text();
    expect(html).toContain(FIRST);
    expect(html).toContain("window.__SSR_PROPS__ = ");
    expect(html).toMatch(/<script[^>]*type="module"/);
    expect(await page.evaluate(() => typeof (window as { __SSR_PROPS__?: unknown }).__SSR_PROPS__)).toBe("object");
    await expect(page.getByRole("heading", { name: "ssr-blog" })).toBeVisible();
    await page.getByRole("link", { name: FIRST }).click();
    await expect(page).toHaveURL(/\/post\/first$/);
    await expect(page.getByRole("heading", { name: FIRST })).toBeVisible();
  });

  test("a post page hydrates: Prev, whose click handler only a hydrated page has, navigates to the previous post", async ({ page }) => {
    const response = await page.goto(`${where.uiUrl}/post/ssg-vs-ssr`);
    expect(response?.status()).toBe(200);
    expect(await response!.text()).toContain(MIDDLE);
    await expect(page.locator("h1")).toHaveText(MIDDLE);
    const prev = page.getByRole("button", { name: "\u2190 Prev" });
    await expect(prev).toBeEnabled();
    await prev.click();
    await expect(page).toHaveURL(/\/post\/first$/);
    await expect(page.locator("h1")).toHaveText(FIRST);
  });
});

describe("ssr-blog under the Vite dev server", () => {
  const dev = target("dev");
  const test = browserTest(dev);

  test("Vite serves the client entry module the dev document loads", async ({ page }) => {
    const response = await page.goto(dev.uiUrl);
    expect(await response!.text()).toContain("/src/entry-client.tsx");
    const entry = await page.request.get(`${dev.uiUrl}/src/entry-client.tsx`);
    expect(entry.status()).toBe(200);
    expect(entry.headers()["content-type"]).toContain("javascript");
  });
});
