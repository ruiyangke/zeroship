/**
 * Dashboard UI E2E tests — tests the React frontend via Playwright browser.
 *
 * Requires: libglib-2.0 (Chromium dependency).
 * Skip on environments without browser support (e.g., minimal Nix shells).
 * Run: npx playwright test e2e/dashboard.spec.ts
 */
import { test, expect } from "@playwright/test";

const MASTER_KEY = "e2e-test-key";

test.describe("Login", () => {
  test("shows login page", async ({ page }) => {
    await page.goto("/");
    await expect(page.locator("text=master key")).toBeVisible();
  });

  test("login with valid key", async ({ page }) => {
    await page.goto("/");
    await page.fill('input[type="password"]', MASTER_KEY);
    await page.click('button[type="submit"]');
    // Should redirect to overview
    await expect(page.locator("text=overview")).toBeVisible({ timeout: 10000 });
  });
});

test.describe("Authenticated", () => {
  test.beforeEach(async ({ page }) => {
    // Login first
    await page.goto("/");
    await page.evaluate((key) => {
      localStorage.setItem("appbase_key", key);
    }, MASTER_KEY);
    await page.goto("/");
  });

  test("overview shows stats", async ({ page }) => {
    await expect(page.locator("text=health")).toBeVisible({ timeout: 10000 });
  });

  test("apps page shows app list", async ({ page }) => {
    await page.click('text=apps');
    await expect(page.locator("text=default")).toBeVisible({ timeout: 10000 });
  });

  test("create app page has form", async ({ page }) => {
    await page.click('text=create app');
    await expect(page.locator('input[placeholder*="app"]')).toBeVisible({ timeout: 10000 });
  });

  test("create app page shows templates", async ({ page }) => {
    await page.click('text=create app');
    // Templates should be visible
    await expect(page.locator("text=Hello World").first()).toBeVisible({ timeout: 10000 });
  });

  test("AI agent page loads", async ({ page }) => {
    await page.click('text=ai agent');
    await expect(page.locator("text=Describe")).toBeVisible({ timeout: 10000 });
  });

  test("app detail shows Monaco editor", async ({ page }) => {
    await page.click('text=apps');
    await page.click('text=default');
    // Monaco editor should load
    await expect(page.locator(".monaco-editor").first()).toBeVisible({ timeout: 15000 });
  });
});
