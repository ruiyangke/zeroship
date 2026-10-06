import { expect as playwrightExpect } from "@playwright/test";
import { launchChromium, pageFixtures, type PageWatch } from "@zeroship/example-testkit";
import type { Browser, Page } from "playwright";
import { afterAll, beforeAll, describe, inject, test as base } from "vitest";
import { target } from "./targets";

const expect = playwrightExpect.configure({ timeout: 15_000 });

describe("ssg-docs deployed through the gateway", () => {
  const deployed = target("deployed");
  let browser: Browser;
  beforeAll(async () => { browser = await launchChromium(); });
  afterAll(async () => { await browser?.close(); });
  const test = base.extend<{ page: Page; watch: PageWatch }>(pageFixtures(() => browser, deployed, inject("ssgArtifacts")));

  test("GET / serves the prerendered home page", async ({ page }) => {
    const response = await page.goto(deployed.uiUrl);
    expect(response?.status()).toBe(200);
    await expect(page.locator(".hero h1")).toHaveText("ssg-docs");
    await expect(page.getByText("built entirely from prerendered HTML")).toBeVisible();
    await expect(page.locator("body")).toContainText("no worker");
  });

  test("GET /about serves the prerendered About page", async ({ page }) => {
    const response = await page.goto(`${deployed.uiUrl}/about`);
    expect(response?.status()).toBe(200);
    await expect(page.locator("h1")).toHaveText("About");
    await expect(page.getByText("no JavaScript, no worker")).toBeVisible();
  });

  test("the pages render the same for a browser that runs no JavaScript", async ({ page }) => {
    const context = await page.context().browser()!.newContext({ javaScriptEnabled: false });
    try {
      const plain = await context.newPage();
      await plain.goto(deployed.uiUrl);
      await expect(plain.locator(".hero h1")).toHaveText("ssg-docs");
      await expect(plain.locator("body")).toContainText("prerendered HTML");
      await plain.goto(`${deployed.uiUrl}/about`);
      await expect(plain.locator("h1")).toHaveText("About");
      await expect(plain.locator("body")).toContainText("no JavaScript, no worker");
    } finally {
      await context.close();
    }
  });

  test("an in-app link loads a new document, because no client router takes it", async ({ page }) => {
    await page.goto(deployed.uiUrl);
    await page.evaluate(() => { (window as unknown as { documentMark?: boolean }).documentMark = true; });
    await page.locator('nav a[href="/about"]').click();
    await expect(page).toHaveURL(/\/about$/);
    await expect(page.locator("h1")).toHaveText("About");
    expect(await page.evaluate(() => (window as unknown as { documentMark?: boolean }).documentMark)).toBeUndefined();
  });
});
