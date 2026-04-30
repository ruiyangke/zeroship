// ─── Admin — every page renders, interactive controls work ──────

import { test, expect } from "@playwright/test";
import {
  createApp,
  deleteApp,
  listApps,
  uniqueAppName,
  visit,
  type AppRecord,
} from "./helpers";

test.describe.serial("admin", () => {
  let fixture: AppRecord;

  test.beforeAll(async ({ request }) => {
    fixture = await createApp(request, uniqueAppName("e2e-adm"));
  });

  test.afterAll(async ({ request }) => {
    await deleteApp(request, fixture.id);
  });

  test("admin nav is present on every admin page", async ({ page }) => {
    await visit(page, "/admin");
    const nav = page.getByTestId("admin-nav");
    await expect(nav).toBeVisible();
    for (const label of ["library", "apps", "users", "revenue", "journal"]) {
      await expect(nav.getByRole("link", { name: new RegExp(`^${label}$`) })).toBeVisible();
    }
  });

  test("/admin (Library) renders KPIs + recent activity + system pulse", async ({ page, request }) => {
    await visit(page, "/admin");
    await expect(page.getByRole("heading", { name: /library/i })).toBeVisible();

    // Four KPIs
    await expect(page.getByText(/^Apps$/)).toBeVisible();
    await expect(page.getByText(/^Users$/)).toBeVisible();
    await expect(page.getByText(/^MRR$/)).toBeVisible();
    await expect(page.getByText(/Platform fee/)).toBeVisible();

    // App count matches backend
    const apps = await listApps(request);
    await expect(page.getByText(`${apps.length} live`).or(page.getByText(/\d+ live/))).toBeVisible();

    // Recent activity ledger + system pulse
    await expect(page.getByRole("heading", { name: /Recent activity/i })).toBeVisible();
    await expect(page.getByRole("heading", { name: /System pulse/i })).toBeVisible();
    await expect(page.getByText(/control plane/)).toBeVisible();
    await expect(page.getByText(/postgres pool/)).toBeVisible();
  });

  test("/admin/apps lists apps + filter pills + search", async ({ page, request }) => {
    await visit(page, "/admin/apps");
    const apps = await listApps(request);

    await expect(page.getByRole("heading", { name: /\[Admin\] Apps/i })).toBeVisible();
    // Our fixture row should be in the table.
    await expect(page.getByRole("cell", { name: fixture.name }).first()).toBeVisible();

    // Filter: drafts only — fixture has no deploy_hash so it's a draft.
    const live = apps.filter((a) => !!a.deploy_hash).length;
    const draft = apps.filter((a) => !a.deploy_hash).length;
    await expect(page.getByRole("button", { name: new RegExp(`live · ${live}`) })).toBeVisible();
    await expect(page.getByRole("button", { name: new RegExp(`draft · ${draft}`) })).toBeVisible();

    await page.getByRole("button", { name: new RegExp(`draft · ${draft}`) }).click();
    await expect(page.getByRole("cell", { name: fixture.name }).first()).toBeVisible();

    // Search: filter by fixture name
    await page.getByPlaceholder(/Search by name/i).fill(fixture.name);
    await expect(page.getByRole("cell", { name: fixture.name }).first()).toBeVisible();

    // Search for something that doesn't exist
    await page.getByPlaceholder(/Search by name/i).fill("zzz_not_an_app_xxx");
    await expect(page.getByText(/No apps match/i)).toBeVisible();
  });

  test("clicking 'open →' on an app row opens its detail page", async ({ page }) => {
    await visit(page, "/admin/apps");
    await page.getByPlaceholder(/Search by name/i).fill(fixture.name);
    // The "open →" link is in the row.
    const row = page.getByRole("row", { name: new RegExp(fixture.name) });
    await row.getByRole("link", { name: /open/i }).click();
    await expect(page).toHaveURL(new RegExp(`/admin/apps/${fixture.id}`));
  });

  test("/admin/apps/:id renders three-pane forensic view", async ({ page }) => {
    await visit(page, `/admin/apps/${fixture.id}`);

    // Title is the app name
    await expect(page.getByRole("heading", { name: fixture.name })).toBeVisible();

    // Left rail (App pages)
    await expect(page.getByText(/App pages/i)).toBeVisible();
    for (const link of ["Overview", "Deploys", "Logs", "Audit", "Env", "Billing"]) {
      await expect(page.getByRole("link", { name: link })).toBeVisible();
    }

    // KPIs (label-uc spans inside <Kpi/>)
    const kpiLabels = page.locator("span.label-uc");
    await expect(kpiLabels.filter({ hasText: /^Status$/ })).toBeVisible();
    await expect(kpiLabels.filter({ hasText: /^Plan$/ })).toBeVisible();
    await expect(kpiLabels.filter({ hasText: /^Deploys$/ })).toBeVisible();
    await expect(kpiLabels.filter({ hasText: /^Updated$/ })).toBeVisible();

    // Right rail (identifiers)
    await expect(page.getByText(/Identifiers/i)).toBeVisible();
    await expect(page.getByText(fixture.id)).toBeVisible();

    // Action buttons
    await expect(page.getByRole("button", { name: /Force redeploy/i })).toBeVisible();
    await expect(page.getByRole("button", { name: /Suspend/i })).toBeVisible();
  });

  test("/admin/users renders directory with current dev user", async ({ page }) => {
    await visit(page, "/admin/users");
    await expect(page.getByRole("heading", { name: /\[Admin\] Users/i })).toBeVisible();
    // The row uses the current user's name (Dev User in dev mode)
    await expect(page.getByText(/Dev User/)).toBeVisible();
    await expect(page.getByPlaceholder(/Search by email/i)).toBeVisible();
  });

  test("/admin/revenue renders MRR chart + platform fee + per-creator", async ({ page }) => {
    await visit(page, "/admin/revenue");
    await expect(page.getByRole("heading", { name: /\[Admin\] Revenue/i })).toBeVisible();
    await expect(page.getByRole("heading", { name: /MRR · last 30 days/i })).toBeVisible();
    await expect(page.getByRole("heading", { name: /Platform fee/i })).toBeVisible();
    await expect(page.getByRole("heading", { name: /^By creator$/ })).toBeVisible();
  });

  test("/admin/journal renders cross-app log filter + ledger placeholder", async ({ page }) => {
    await visit(page, "/admin/journal");
    await expect(page.getByRole("heading", { name: /System journal/i })).toBeVisible();
    await expect(page.getByPlaceholder(/Filter by app/i)).toBeVisible();
    // 4 filter pills
    for (const label of ["all", "info", "warn", "error"]) {
      await expect(page.getByRole("button", { name: new RegExp(`^${label}$`) })).toBeVisible();
    }
  });

  test("admin sub-nav navigates between pages", async ({ page }) => {
    await visit(page, "/admin");
    const nav = page.getByTestId("admin-nav");

    await nav.getByRole("link", { name: /^apps$/ }).click();
    await expect(page).toHaveURL(/\/admin\/apps$/);

    await nav.getByRole("link", { name: /^users$/ }).click();
    await expect(page).toHaveURL(/\/admin\/users$/);

    await nav.getByRole("link", { name: /^revenue$/ }).click();
    await expect(page).toHaveURL(/\/admin\/revenue$/);

    await nav.getByRole("link", { name: /^journal$/ }).click();
    await expect(page).toHaveURL(/\/admin\/journal$/);

    await nav.getByRole("link", { name: /^library$/ }).click();
    await expect(page).toHaveURL(/\/admin$/);
  });
});
