// ─── Routing — default tab, 404 fallback, auth pages ─────────────

import { test, expect } from "@playwright/test";
import { listApps, visit } from "./helpers";

test.describe("routing", () => {
  test("/p/:appId (no tab) → /p/:appId/preview", async ({ page, request }) => {
    const apps = await listApps(request);
    test.skip(apps.length === 0, "no app");
    const id = apps[0].id;

    await visit(page, `/p/${id}`);
    await expect(page).toHaveURL(new RegExp(`/p/${id}/preview$`));
  });

  test("unknown path falls through to /", async ({ page }) => {
    await visit(page, "/some/totally/fake/path");
    await expect(page).toHaveURL(/\/$/);
  });

  test("/login renders login page (devBypass redirects authed users back)", async ({ page }) => {
    // In dev, the synthetic "Dev User" is always logged in, so RedirectIfAuthed
    // sends /login → "/". Verify either (a) the redirect happened, or
    // (b) the form rendered if devBypass is off.
    await visit(page, "/login");
    const url = page.url();
    if (url.endsWith("/login")) {
      await expect(page.getByTestId("login-page")).toBeVisible();
    } else {
      await expect(page).toHaveURL(/\/$/);
    }
  });

  test("/signup renders signup page (devBypass redirects authed users back)", async ({ page }) => {
    await visit(page, "/signup");
    const url = page.url();
    if (url.endsWith("/signup")) {
      await expect(page.getByTestId("signup-page")).toBeVisible();
    } else {
      await expect(page).toHaveURL(/\/$/);
    }
  });
});
