/**
 * Dashboard UI E2E tests — comprehensive browser tests.
 *
 * Requires: Chromium (via Nix playwright-test or npx playwright install)
 * Servers: appbase on :3335, vite on :5173
 */
import { test, expect, type Page } from "@playwright/test";

const MASTER_KEY = "e2e-test-key";
const API = "http://localhost:3335";

/** Helper: login and navigate to authenticated state */
async function login(page: Page) {
  await page.goto("/");
  await page.evaluate((key) => localStorage.setItem("appbase_key", key), MASTER_KEY);
  await page.goto("/");
  await expect(page.locator("text=overview").first()).toBeVisible({ timeout: 10000 });
}

/** Helper: create an app via API for test setup */
async function createTestApp(request: any, id: string, code?: string) {
  await request.post(`${API}/api/apps`, {
    headers: { Authorization: `Bearer ${MASTER_KEY}`, "Content-Type": "application/json" },
    data: { id, plan_id: "free" },
  });
  if (code) {
    await request.post(`${API}/api/apps/${id}/deploy`, {
      headers: { Authorization: `Bearer ${MASTER_KEY}`, "Content-Type": "application/javascript" },
      data: code,
    });
  }
}

/** Helper: delete an app via API for cleanup */
async function deleteTestApp(request: any, id: string) {
  await request.delete(`${API}/api/apps/${id}`, {
    headers: { Authorization: `Bearer ${MASTER_KEY}` },
  });
}

// =========================================================================
// Login
// =========================================================================

test.describe("Login Page", () => {
  test("shows login form with key input", async ({ page }) => {
    await page.goto("/");
    const input = page.locator('input[type="password"]');
    await expect(input).toBeVisible();
    const button = page.locator('button[type="submit"]');
    await expect(button).toBeVisible();
  });

  test("login with valid key redirects to overview", async ({ page }) => {
    await page.goto("/");
    await page.fill('input[type="password"]', MASTER_KEY);
    await page.click('button[type="submit"]');
    await expect(page.locator("text=overview").first()).toBeVisible({ timeout: 10000 });
    // Verify key is stored
    const key = await page.evaluate(() => localStorage.getItem("appbase_key"));
    expect(key).toBe(MASTER_KEY);
  });

  test("login persists across page reloads", async ({ page }) => {
    await login(page);
    await page.reload();
    // Should still be authenticated (no login page)
    await expect(page.locator("text=overview").first()).toBeVisible({ timeout: 10000 });
  });
});

// =========================================================================
// Overview
// =========================================================================

test.describe("Overview Page", () => {
  test.beforeEach(async ({ page }) => { await login(page); });

  test("shows health status", async ({ page }) => {
    await expect(page.locator("text=health").first()).toBeVisible({ timeout: 10000 });
  });

  test("shows pool stats", async ({ page }) => {
    await expect(page.locator("text=isolate").first()).toBeVisible({ timeout: 10000 });
  });

  test("sidebar navigation works", async ({ page }) => {
    // Click through all nav items
    await page.click("text=apps");
    await expect(page).toHaveURL(/\/apps/);

    await page.click("text=create app");
    await expect(page).toHaveURL(/\/apps\/new/);

    await page.click("text=ai agent");
    await expect(page).toHaveURL(/\/ai/);

    await page.click("text=overview");
    await expect(page).toHaveURL(/\/$/);
  });
});

// =========================================================================
// App List
// =========================================================================

test.describe("App List Page", () => {
  test.beforeEach(async ({ page }) => { await login(page); });

  test("shows default app", async ({ page }) => {
    await page.click("text=apps");
    await expect(page.locator("text=default").first()).toBeVisible({ timeout: 10000 });
  });

  test("clicking app navigates to detail", async ({ page }) => {
    await page.click("text=apps");
    await page.click("text=default");
    await expect(page).toHaveURL(/\/apps\/default/);
  });
});

// =========================================================================
// Create App
// =========================================================================

test.describe("Create App Page", () => {
  test.beforeEach(async ({ page }) => { await login(page); });

  test("shows form with app id input and plan select", async ({ page }) => {
    await page.click("text=create app");
    await expect(page.locator('input[placeholder*="app"]').first()).toBeVisible({ timeout: 10000 });
  });

  test("shows starter templates", async ({ page }) => {
    await page.click("text=create app");
    // Templates load from API — look for any template name
    await expect(page.locator("text=hello-world").first()).toBeVisible({ timeout: 10000 });
  });

  test("can create app with form", async ({ page, request }) => {
    const appId = `ui-create-${Date.now()}`;
    await page.click("text=create app");

    await page.fill('input[placeholder*="app"]', appId);
    await page.click('button:has-text("create")');

    // Should navigate to app detail
    await expect(page).toHaveURL(new RegExp(`/apps/${appId}`), { timeout: 10000 });

    // Cleanup
    await deleteTestApp(request, appId);
  });
});

// =========================================================================
// App Detail
// =========================================================================

test.describe("App Detail Page", () => {
  // Use the "default" app which always exists
  const appId = "default";

  test.beforeEach(async ({ page }) => { await login(page); });

  test("shows app info", async ({ page }) => {
    await page.goto(`/apps/${appId}`);
    await expect(page.locator(`text=${appId}`).first()).toBeVisible({ timeout: 10000 });
  });

  test("shows Monaco editor with deployed code", async ({ page }) => {
    await page.goto(`/apps/${appId}`);
    await expect(page.locator(".monaco-editor").first()).toBeVisible({ timeout: 15000 });
  });

  test("shows API key section", async ({ page }) => {
    await page.goto(`/apps/${appId}`);
    // API key section should exist
    await expect(page.locator("text=api key").first()).toBeVisible({ timeout: 10000 });
    // Should have show/hide button
    await expect(page.locator("text=show").first()).toBeVisible({ timeout: 5000 });
  });

  test("shows quick start curl command", async ({ page }) => {
    await page.goto(`/apps/${appId}`);
    await expect(page.locator("text=curl").first()).toBeVisible({ timeout: 10000 });
  });

  test("test panel: can call RPC method", async ({ page }) => {
    await page.goto(`/apps/${appId}`);

    // Method input has placeholder "e.g. add"
    const methodInput = page.locator('#rpc-method, input[placeholder="e.g. add"]').first();
    await expect(methodInput).toBeVisible({ timeout: 10000 });
    await methodInput.fill("ping");

    // Click run button (lowercase "run")
    const runBtn = page.locator('button:has-text("run")').first();
    await expect(runBtn).toBeVisible({ timeout: 5000 });
    await runBtn.click();

    // Should see "pong" in result area
    await expect(page.locator("text=pong").first()).toBeVisible({ timeout: 10000 });
  });

  test("deploy: deploy button exists", async ({ page }) => {
    await page.goto(`/apps/${appId}`);

    // Wait for Monaco to load
    await expect(page.locator(".monaco-editor").first()).toBeVisible({ timeout: 15000 });

    // Deploy button should be visible (text may vary: "deploy", "DEPLOY", etc.)
    const deployBtn = page.locator('button').filter({ hasText: /deploy/i }).first();
    await expect(deployBtn).toBeVisible({ timeout: 5000 });
  });
});

// =========================================================================
// App Detail — Extended Flows (create a test app, exercise full lifecycle)
// =========================================================================

test.describe("App Lifecycle in Dashboard", () => {
  const appId = `lifecycle-${Date.now()}`;

  test.beforeEach(async ({ page }) => { await login(page); });

  test("navigate to created app and see details", async ({ page, request }) => {
    // Create app via API
    await createTestApp(request, appId,
      'var __rpc = { greet: function(name) { return "Hi " + name; } };'
    );

    // Navigate to app detail
    await page.goto(`/apps/${appId}`);
    await expect(page.locator(`text=${appId}`).first()).toBeVisible({ timeout: 10000 });

    // Monaco editor should show code
    await expect(page.locator(".monaco-editor").first()).toBeVisible({ timeout: 15000 });

    // Cleanup
    await deleteTestApp(request, appId);
  });

  test("app detail page has delete button", async ({ page, request }) => {
    const delAppId = `del-${Date.now()}`;
    await createTestApp(request, delAppId);

    await page.goto(`/apps/${delAppId}`);
    await expect(page.locator(`text=${delAppId}`).first()).toBeVisible({ timeout: 10000 });

    // Delete button should exist in danger zone
    const deleteBtn = page.locator('button').filter({ hasText: /delete/i }).first();
    await expect(deleteBtn).toBeVisible({ timeout: 5000 });

    // Cleanup via API
    await deleteTestApp(request, delAppId);
  });
});

// =========================================================================
// AI Chat
// =========================================================================

test.describe("AI Chat Page", () => {
  test.beforeEach(async ({ page }) => { await login(page); });

  test("shows chat interface with example prompts", async ({ page }) => {
    await page.click("text=ai agent");
    await expect(page.locator("text=Describe").first()).toBeVisible({ timeout: 10000 });
    // Should show example prompts
    await expect(page.locator("text=todo").first()).toBeVisible({ timeout: 5000 });
  });

  test("clicking example fills input", async ({ page }) => {
    await page.click("text=ai agent");
    // Click first example prompt
    const example = page.locator("button:has-text('todo')").first();
    await expect(example).toBeVisible({ timeout: 5000 });
    await example.click();

    // Input should be filled
    const textarea = page.locator("textarea").first();
    const value = await textarea.inputValue();
    expect(value.length).toBeGreaterThan(0);
  });
});

// =========================================================================
// Logout
// =========================================================================

test.describe("Logout", () => {
  test("logout clears auth and shows login", async ({ page }) => {
    await login(page);
    await page.click("text=logout");
    // Should show login page
    await expect(page.locator('input[type="password"]')).toBeVisible({ timeout: 5000 });
    // Key should be cleared
    const key = await page.evaluate(() => localStorage.getItem("appbase_key"));
    expect(key).toBeNull();
  });
});
