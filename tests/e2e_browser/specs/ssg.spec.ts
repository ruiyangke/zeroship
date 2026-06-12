import { test, expect } from "@playwright/test";
import { appSlug, appUrl } from "../helpers";

// SSG (ssg-docs): three HTML files, NO worker, NO client JavaScript. The deploy
// manifest's `worker` field is null and every rule is a Static action. The
// browser contract we assert here:
//   1. the prerendered content is in the real DOM (gateway served static bytes)
//   2. it renders identically with JavaScript DISABLED (truly static — no
//      hydration, no client framework)
//   3. an in-app <nav> link performs a real, full-page navigation (because
//      there is no SPA router to intercept it)
test.describe("SSG (ssg-docs) — prerendered, zero-JS static", () => {
  test.skip(!appSlug("ssg"), "ssg-docs not deployed (dist missing)");

  test("GET / serves prerendered hero content in the DOM", async ({ page }) => {
    await page.goto(appUrl("ssg", "/"), { waitUntil: "domcontentloaded" });

    await expect(page.locator(".hero h1")).toHaveText("ssg-docs");
    await expect(
      page.getByText("built entirely from prerendered HTML", { exact: false }),
    ).toBeVisible();
    // The "no worker" claim is the literal copy in the source page.
    await expect(page.locator("body")).toContainText("no worker");
  });

  test("GET /about serves the prerendered About prose", async ({ page }) => {
    await page.goto(appUrl("ssg", "/about"), { waitUntil: "domcontentloaded" });

    await expect(page.locator("h1")).toHaveText("About");
    await expect(
      page.getByText("no JavaScript, no worker", { exact: false }),
    ).toBeVisible();
  });

  test("renders identically with JavaScript DISABLED (truly static)", async ({ browser }) => {
    // A fresh context with JS turned off proves the page needs zero client code:
    // an SSG page renders exactly the same; a CSR/SSR page would lose its mounted
    // tree. This is the load-bearing "SSG is static" assertion.
    const ctx = await browser.newContext({ javaScriptEnabled: false });
    const page = await ctx.newPage();
    try {
      await page.goto(appUrl("ssg", "/"), { waitUntil: "domcontentloaded" });
      await expect(page.locator(".hero h1")).toHaveText("ssg-docs");
      await expect(page.locator("body")).toContainText("prerendered HTML");

      await page.goto(appUrl("ssg", "/about"), { waitUntil: "domcontentloaded" });
      await expect(page.locator("h1")).toHaveText("About");
      await expect(page.locator("body")).toContainText("no JavaScript, no worker");
    } finally {
      await ctx.close();
    }
  });

  test("in-app nav link does a full document navigation (no SPA router)", async ({ page }) => {
    await page.goto(appUrl("ssg", "/"), { waitUntil: "domcontentloaded" });

    // Mark the current document; a full reload (real navigation) wipes the mark.
    await page.evaluate(() => {
      (window as unknown as { __ssgMark?: boolean }).__ssgMark = true;
    });

    await Promise.all([
      page.waitForURL(/\/about$/),
      page.locator('nav a[href="/about"]').click(),
    ]);

    await expect(page.locator("h1")).toHaveText("About");
    // The mark is gone ⇒ this was a genuine document load, not a client-side
    // route swap. (SSG ships no router; the gateway served /about.html fresh.)
    const markSurvived = await page.evaluate(
      () => (window as unknown as { __ssgMark?: boolean }).__ssgMark === true,
    );
    expect(markSurvived).toBe(false);
  });
});
