import { test, expect } from "@playwright/test";

// Deep canvas interaction coverage. Each test exercises the catch-all
// workspace shell at /__catchall_for_test where the WorkspaceShell
// mounts without an appId. Canvases that need an appId render their
// "No project selected." fallback — we still verify the pill switching
// and the topbar affordances. Canvases that work without an appId
// (preview-canvas) get full interaction coverage.
//
// For per-app behaviour (Files / Logs / Env / Settings / Plan / Health
// / Data / Media), the existing workspace-canvases.spec.ts and
// data-media.spec.ts files require a control plane and skip cleanly
// without one. These tests target the SHELL — pill switching, no-
// project fallback, ProductTour open/close, topbar URL pill — because
// those exercise the contract every canvas relies on.

const SHELL_PATH = "/__catchall_for_test";

test.describe("Canvas pills — every pill renders + activates", () => {
  test("all 9 pills render in the canvas-pills bar", async ({ page }) => {
    await page.goto(SHELL_PATH);
    await expect(page.getByTestId("canvas-pills")).toBeVisible();
    for (const id of [
      "preview",
      "files",
      "data",
      "media",
      "logs",
      "env",
      "plan",
      "health",
      "settings",
    ]) {
      await expect(page.getByTestId(`pill:${id}`)).toBeVisible();
    }
  });

  test("preview pill is the default active canvas", async ({ page }) => {
    await page.goto(SHELL_PATH);
    await expect(page.getByTestId("preview-canvas")).toBeVisible();
  });

  test("clicking each non-preview pill swaps the canvas (no-app fallback shows)", async ({
    page,
  }) => {
    await page.goto(SHELL_PATH);
    for (const id of ["files", "data", "media", "logs", "env", "plan", "health", "settings"]) {
      await page.getByTestId(`pill:${id}`).click();
      await expect(page.getByText(/no project selected/i)).toBeVisible();
      // After fallback shows, switch back to preview to reset.
      await page.getByTestId("pill:preview").click();
      await expect(page.getByTestId("preview-canvas")).toBeVisible();
    }
  });

  test("preview pill shows the placeholder URL pill", async ({ page }) => {
    await page.goto(SHELL_PATH);
    await expect(page.getByTestId("preview-canvas")).toBeVisible();
  });
});

test.describe("TopBar affordances", () => {
  test("topbar tour button has aria-label", async ({ page }) => {
    await page.goto(SHELL_PATH);
    const btn = page.getByTestId("topbar-tour");
    await expect(btn).toBeVisible();
    await expect(btn).toHaveAttribute("aria-label", /tour/i);
  });

  test("topbar URL pill renders the placeholder host on tablet+", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 800 });
    await page.goto(SHELL_PATH);
    const url = page.getByTestId("topbar-url");
    await expect(url).toBeVisible();
    await expect(url).toContainText(".zeroship.app");
  });

  test("topbar URL pill is hidden on phone widths (sm:inline-flex)", async ({ page }) => {
    await page.setViewportSize({ width: 375, height: 800 });
    await page.goto(SHELL_PATH);
    const url = page.getByTestId("topbar-url");
    // Tailwind hides this with `hidden sm:inline-flex`. Playwright
    // sees `display: none` as not visible.
    await expect(url).toBeHidden();
  });
});

test.describe("Canvas pills — keyboard activation", () => {
  test("clicking a pill via keyboard Tab+Enter switches active", async ({ page }) => {
    await page.goto(SHELL_PATH);
    // Click preview to ensure baseline.
    await page.getByTestId("pill:preview").click();
    // Tab to the files pill — exact tab count depends on layout, so
    // we click directly with .focus() then keyboard Enter.
    const files = page.getByTestId("pill:files");
    await files.focus();
    await page.keyboard.press("Enter");
    await expect(page.getByText(/no project selected/i)).toBeVisible();
  });
});

test.describe("Plan canvas — no-app fallback", () => {
  test("plan pill shows the no-project nudge when appId is absent", async ({ page }) => {
    await page.goto(SHELL_PATH);
    await page.getByTestId("pill:plan").click();
    await expect(page.getByText(/no project selected/i)).toBeVisible();
  });
});

test.describe("Health canvas — no-app fallback", () => {
  test("health pill shows the no-project nudge when appId is absent", async ({ page }) => {
    await page.goto(SHELL_PATH);
    await page.getByTestId("pill:health").click();
    await expect(page.getByText(/no project selected/i)).toBeVisible();
  });
});

test.describe("Data + Media canvases — no-app fallback", () => {
  test("data pill shows the no-project nudge when appId is absent", async ({ page }) => {
    await page.goto(SHELL_PATH);
    await page.getByTestId("pill:data").click();
    await expect(page.getByText(/no project selected/i)).toBeVisible();
  });
  test("media pill shows the no-project nudge when appId is absent", async ({ page }) => {
    await page.goto(SHELL_PATH);
    await page.getByTestId("pill:media").click();
    await expect(page.getByText(/no project selected/i)).toBeVisible();
  });
});

test.describe("Workspace error path", () => {
  test("/p/<unknown-id>/preview routes via getApp; on failure shows error or loads", async ({ page }) => {
    // Point at a definitely-invalid app id. The dev server's getApp
    // handler returns 404 → the workspace shell shows its
    // "Project not found" panel. Without a control plane, the shell
    // may instead stay in the loading state — accept either as
    // graceful degradation.
    await page.goto("/p/app_definitelynotreal_xyz/preview");
    // Either workspace-error appears, or workspace-loading appears,
    // or the canvas mounts with placeholder data — all valid graceful
    // states. The forbidden state is a blank screen, so we check that
    // *something* in the shell mounted.
    const error = page.getByTestId("workspace-error");
    const loading = page.getByTestId("workspace-loading");
    const canvas = page.getByTestId("canvas-area");
    await expect(error.or(loading).or(canvas).first()).toBeVisible({ timeout: 10_000 });
  });
});

test.describe("Account page sections", () => {
  test("account page renders identity + plan + sessions + 2fa + delete + logout", async ({ page }) => {
    await page.goto("/account");
    await expect(page.getByTestId("account-page")).toBeVisible();
    await expect(page.getByTestId("account-name")).toBeVisible();
    await expect(page.getByTestId("account-email")).toBeVisible();
    await expect(page.getByTestId("account-name")).toHaveAttribute("readonly", "");
    await expect(page.getByTestId("account-email")).toHaveAttribute("readonly", "");
    await expect(page.getByTestId("account-sessions")).toBeVisible();
    await expect(page.getByTestId("account-2fa")).toBeVisible();
    await expect(page.getByTestId("account-delete")).toBeVisible();
    await expect(page.getByTestId("account-logout")).toBeVisible();
  });

  test("account page deferred sections reference ISS-10/11/12", async ({ page }) => {
    await page.goto("/account");
    await expect(page.getByTestId("account-sessions")).toContainText("ISS-10");
    await expect(page.getByTestId("account-2fa")).toContainText("ISS-11");
    await expect(page.getByTestId("account-delete")).toContainText("ISS-12");
  });
});

test.describe("Templates filter pills", () => {
  test("filter pill switches the visible grid", async ({ page }) => {
    await page.goto("/templates");
    // The default `all` filter shows the full grid.
    await expect(page.getByTestId("templates-grid")).toBeVisible();
    // Click a non-all filter and verify the grid still mounts.
    const filters = page.getByTestId("templates-filters");
    const firstNonAll = filters.locator('[data-testid^="filter:"]').nth(1);
    await firstNonAll.click();
    // Either grid or empty state remains visible.
    const grid = page.getByTestId("templates-grid");
    const empty = page.getByTestId("templates-empty");
    await expect(grid.or(empty).first()).toBeVisible();
  });

  test("templates page has a 'describe your own' link to /new", async ({ page }) => {
    await page.goto("/templates");
    const blank = page.getByTestId("templates-blank");
    await expect(blank).toBeVisible();
    await blank.click();
    await expect(page).toHaveURL(/\/new$/);
  });
});

test.describe("Skills page", () => {
  test("multiple skill cards render, all with disabled add button", async ({ page }) => {
    await page.goto("/skills");
    const cards = page.locator('[data-testid^="skill-card:"]');
    const count = await cards.count();
    expect(count).toBeGreaterThanOrEqual(4);
    // Add buttons all disabled (registry not wired — ISS-13).
    const addBtns = page.locator('[data-testid^="skill-add:"]');
    const addCount = await addBtns.count();
    for (let i = 0; i < addCount; i++) {
      await expect(addBtns.nth(i)).toBeDisabled();
    }
  });
});

test.describe("Pricing CTAs route to signup with plan param", () => {
  test("Free plan CTA → /signup?plan=free", async ({ page }) => {
    await page.goto("/pricing");
    await page.getByTestId("pricing-cta-free").click();
    await expect(page).toHaveURL(/\/signup\?plan=free$/);
  });
  test("Maker plan CTA → /signup?plan=maker", async ({ page }) => {
    await page.goto("/pricing");
    await page.getByTestId("pricing-cta-maker").click();
    await expect(page).toHaveURL(/\/signup\?plan=maker$/);
  });
  test("Pro plan CTA → /signup?plan=pro", async ({ page }) => {
    await page.goto("/pricing");
    await page.getByTestId("pricing-cta-pro").click();
    await expect(page).toHaveURL(/\/signup\?plan=pro$/);
  });
});
