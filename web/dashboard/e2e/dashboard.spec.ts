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
    await expect(page.locator("text=Hello World").first()).toBeVisible({ timeout: 10000 });
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

  test("shows API key (masked)", async ({ page }) => {
    await page.goto(`/apps/${appId}`);
    // API key should be masked with asterisks
    await expect(page.locator("text=****").first()).toBeVisible({ timeout: 10000 });
  });

  test("shows quick start curl command", async ({ page }) => {
    await page.goto(`/apps/${appId}`);
    await expect(page.locator("text=curl").first()).toBeVisible({ timeout: 10000 });
  });

  test("test panel: can call RPC method", async ({ page }) => {
    await page.goto(`/apps/${appId}`);

    // Find method input and fill it — default app has "ping"
    const methodInput = page.locator('input[placeholder*="method"]').first();
    await expect(methodInput).toBeVisible({ timeout: 10000 });
    await methodInput.fill("ping");

    // Click run button
    await page.click('button:has-text("run")');

    // Should see "pong" result
    await expect(page.locator("text=pong").first()).toBeVisible({ timeout: 10000 });
  });

  test("deploy: can redeploy code", async ({ page }) => {
    await page.goto(`/apps/${appId}`);

    // Wait for Monaco to load
    await expect(page.locator(".monaco-editor").first()).toBeVisible({ timeout: 15000 });

    // Click deploy button
    const deployBtn = page.locator('button:has-text("deploy"), button:has-text("DEPLOY")').first();
    await expect(deployBtn).toBeVisible({ timeout: 5000 });
    await deployBtn.click();

    // Should see version increment or success message
    await expect(page.locator("text=v").first()).toBeVisible({ timeout: 10000 });
  });
});

// =========================================================================
// App Detail — Extended Flows (create a test app, exercise full lifecycle)
// =========================================================================

test.describe("App Lifecycle in Dashboard", () => {
  const appId = `lifecycle-${Date.now()}`;

  test.beforeEach(async ({ page }) => { await login(page); });

  test("full lifecycle: create → deploy → test → logs → delete", async ({ page, request }) => {
    // Step 1: Create app via API (faster than UI for setup)
    await createTestApp(request, appId,
      'var __rpc = { greet: function(name) { console.log("Hello " + name); return "Hi " + name; } };'
    );

    // Step 2: Navigate to app detail
    await page.goto(`/apps/${appId}`);
    await expect(page.locator(`text=${appId}`).first()).toBeVisible({ timeout: 10000 });

    // Step 3: Test panel — call RPC
    const methodInput = page.locator('input[placeholder*="method"]').first();
    await expect(methodInput).toBeVisible({ timeout: 10000 });
    await methodInput.fill("greet");

    const paramsInput = page.locator('textarea[placeholder*="param"], input[placeholder*="param"]').first();
    if (await paramsInput.isVisible({ timeout: 2000 }).catch(() => false)) {
      await paramsInput.fill('["World"]');
    }

    await page.click('button:has-text("run")');
    await expect(page.locator("text=Hi World").first()).toBeVisible({ timeout: 10000 });

    // Step 4: Logs should show console.log output
    // The logs section auto-refreshes — wait for it
    await expect(page.locator("text=Hello World").first()).toBeVisible({ timeout: 15000 });

    // Cleanup
    await deleteTestApp(request, appId);
  });

  test("delete app from app list", async ({ page, request }) => {
    const delAppId = `del-${Date.now()}`;
    await createTestApp(request, delAppId);

    // Navigate to apps list
    await page.goto("/apps");
    await expect(page.locator(`text=${delAppId}`).first()).toBeVisible({ timeout: 10000 });

    // Navigate to app detail
    await page.click(`text=${delAppId}`);
    await expect(page).toHaveURL(new RegExp(`/apps/${delAppId}`), { timeout: 5000 });

    // Find and click delete button
    const deleteBtn = page.locator('button:has-text("delete"), button:has-text("DELETE")').first();
    await expect(deleteBtn).toBeVisible({ timeout: 5000 });

    // Handle confirmation dialog
    page.on("dialog", (dialog) => dialog.accept());
    await deleteBtn.click();

    // Should navigate away (to apps list or show deleted message)
    await page.waitForTimeout(2000);
    // Verify app is gone from API
    const res = await request.get(`${API}/api/apps/${delAppId}`, {
      headers: { Authorization: `Bearer ${MASTER_KEY}` },
    });
    expect(res.status()).toBe(404);
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
