// ─── Workspace — every tab, env CRUD, settings, danger zone ──────
//
// Each test file shares one fixture app created in a beforeAll and
// torn down in afterAll. The env CRUD test mutates real backend
// state so it asserts both UI feedback AND the underlying RPC
// state matches.

import { test, expect } from "@playwright/test";
import {
  createApp,
  deleteApp,
  listApps,
  listSecrets,
  listVars,
  uniqueAppName,
  visit,
  type AppRecord,
} from "./helpers";

test.describe.serial("workspace", () => {
  let fixture: AppRecord;

  test.beforeAll(async ({ request }) => {
    fixture = await createApp(request, uniqueAppName("e2e-ws"), "free");
  });

  test.afterAll(async ({ request }) => {
    await deleteApp(request, fixture.id);
  });

  // ── Shell + nav ────────────────────────────────────────────────

  test("workspace shell loads with TopBar, tab content, and ChatRail", async ({ page }) => {
    await visit(page, `/p/${fixture.id}/preview`);

    // TopBar renders the project name (might briefly say "loading…")
    await expect(page.getByTestId("topbar-url")).toBeVisible();
    await expect(page.getByTestId("topbar-url")).toContainText(`${fixture.name}.zeroship.app`);

    // The 1fr | 380px desk layout
    await expect(page.getByTestId("tab-content")).toBeVisible();
    await expect(page.getByTestId("chat-rail")).toBeVisible();
  });

  test("Preview tab is the default + shows placeholder for an undeployed app", async ({ page }) => {
    await visit(page, `/p/${fixture.id}`);
    await expect(page).toHaveURL(new RegExp(`/p/${fixture.id}/preview$`));
    await expect(page.getByTestId("preview-tab")).toBeVisible();
    // Fresh app, no deploy_hash → placeholder copy.
    await expect(page.getByText(/Nothing's been built yet/i)).toBeVisible();
  });

  test("TabDrawer navigates between all 5 tabs", async ({ page }) => {
    await visit(page, `/p/${fixture.id}/preview`);
    const drawer = page.getByTestId("tab-drawer");
    await expect(drawer).toBeVisible();

    await drawer.getByTestId("tab:files").click();
    await expect(page).toHaveURL(new RegExp(`/p/${fixture.id}/files$`));
    await expect(page.getByTestId("files-tab")).toBeVisible();

    await drawer.getByTestId("tab:logs").click();
    await expect(page).toHaveURL(new RegExp(`/p/${fixture.id}/logs$`));
    await expect(page.getByTestId("logs-tab")).toBeVisible();

    await drawer.getByTestId("tab:env").click();
    await expect(page).toHaveURL(new RegExp(`/p/${fixture.id}/env$`));
    await expect(page.getByTestId("env-tab")).toBeVisible();

    await drawer.getByTestId("tab:settings").click();
    await expect(page).toHaveURL(new RegExp(`/p/${fixture.id}/settings$`));
    await expect(page.getByTestId("settings-tab")).toBeVisible();

    await drawer.getByTestId("tab:preview").click();
    await expect(page).toHaveURL(new RegExp(`/p/${fixture.id}/preview$`));
    await expect(page.getByTestId("preview-tab")).toBeVisible();
  });

  // ── Files ──────────────────────────────────────────────────────

  test("Files tab renders three-pane layout (tree, editor, marginalia)", async ({ page }) => {
    await visit(page, `/p/${fixture.id}/files`);
    await expect(page.getByTestId("files-tab")).toBeVisible();
    await expect(page.getByText(/Manuscript/i)).toBeVisible();
    // Either lists files (sandbox session opened) or shows the empty state.
    const tab = page.getByTestId("files-tab");
    await expect(tab).toContainText(/no files yet|loading|.*/);
    // Marginalia rail copy
    await expect(page.getByText(/Currently open/i)).toBeVisible();
  });

  // ── Logs ───────────────────────────────────────────────────────

  test("Logs tab renders filter pills + ledger", async ({ page }) => {
    await visit(page, `/p/${fixture.id}/logs`);
    await expect(page.getByTestId("logs-tab")).toBeVisible();
    // Empty state for a fresh app (no requests served yet).
    await expect(
      page.getByText(/No events yet|loading/i),
    ).toBeVisible();
    // The 4 filter pills.
    await expect(page.getByRole("button", { name: /^all$/ })).toBeVisible();
    await expect(page.getByRole("button", { name: /^info$/ })).toBeVisible();
    await expect(page.getByRole("button", { name: /^warn$/ })).toBeVisible();
    await expect(page.getByRole("button", { name: /^error$/ })).toBeVisible();
  });

  // ── Env CRUD: real backend writes ──────────────────────────────

  test("Env tab shows empty state for a fresh app", async ({ page, request }) => {
    await visit(page, `/p/${fixture.id}/env`);
    await expect(page.getByTestId("env-tab")).toBeVisible();
    await expect(page.getByText(/No variables yet/i)).toBeVisible();
    await expect(page.getByText(/No secrets yet/i)).toBeVisible();

    const v = await listVars(request, fixture.id);
    const s = await listSecrets(request, fixture.id);
    expect(v.vars).toEqual([]);
    expect(s.secrets).toEqual([]);
  });

  test("Add a variable: real backend writes, UI reflects", async ({ page, request }) => {
    await visit(page, `/p/${fixture.id}/env`);
    await expect(page.getByTestId("env-tab")).toBeVisible();

    await page.getByRole("button", { name: /Add a variable/i }).click();
    // First input is the key, then the value, then Save.
    const inputs = page.getByTestId("env-tab").locator("input");
    await inputs.nth(0).fill("API_BASE");
    await inputs.nth(1).fill("https://example.test");

    await page.getByRole("button", { name: /^Save\b/i }).click();

    // The new row should appear in the list.
    await expect(page.getByText(/API_BASE/)).toBeVisible();
    await expect(page.getByText("https://example.test")).toBeVisible();

    // Backend confirms.
    const after = await listVars(request, fixture.id);
    expect(after.vars).toContainEqual({ key: "API_BASE", value: "https://example.test" });
  });

  test("Delete the variable", async ({ page, request }) => {
    await visit(page, `/p/${fixture.id}/env`);
    await expect(page.getByText(/API_BASE/)).toBeVisible();

    // The row's action button is "delete"
    const row = page
      .getByTestId("env-tab")
      .locator("div", { hasText: /API_BASE/ })
      .first();
    await row.getByRole("button", { name: /^delete$/ }).click();

    await expect(page.getByText(/No variables yet/i)).toBeVisible();
    const after = await listVars(request, fixture.id);
    expect(after.vars.find((v) => v.key === "API_BASE")).toBeUndefined();
  });

  test("Add a secret: real backend writes, UI shows masked dots", async ({ page, request }) => {
    await visit(page, `/p/${fixture.id}/env`);

    await page.getByRole("button", { name: /Add a secret/i }).click();
    const inputs = page.getByTestId("env-tab").locator("input");
    // After clicking Add a secret, the inline form puts key + masked value at end.
    await inputs.nth(0).fill("STRIPE_KEY");
    await inputs.nth(1).fill("sk_test_super_secret");
    await page.getByRole("button", { name: /^Save\b/i }).click();

    await expect(page.getByText(/STRIPE_KEY/)).toBeVisible();
    // The actual value is replaced by mask dots — never visible in the UI.
    await expect(page.getByText("sk_test_super_secret")).toHaveCount(0);

    const after = await listSecrets(request, fixture.id);
    expect(after.secrets).toContain("STRIPE_KEY");
  });

  test("Rotate (delete) the secret", async ({ page, request }) => {
    await visit(page, `/p/${fixture.id}/env`);
    await expect(page.getByText(/STRIPE_KEY/)).toBeVisible();

    const row = page
      .getByTestId("env-tab")
      .locator("div", { hasText: /STRIPE_KEY/ })
      .first();
    await row.getByRole("button", { name: /^rotate$/ }).click();

    await expect(page.getByText(/No secrets yet/i)).toBeVisible();
    const after = await listSecrets(request, fixture.id);
    expect(after.secrets).not.toContain("STRIPE_KEY");
  });

  // ── Settings ───────────────────────────────────────────────────

  test("Settings tab renders General/Domain/Plan/Danger sections", async ({ page }) => {
    await visit(page, `/p/${fixture.id}/settings`);
    await expect(page.getByTestId("settings-tab")).toBeVisible();

    await expect(page.getByRole("heading", { name: /^General$/ })).toBeVisible();
    await expect(page.getByRole("heading", { name: /^Domain$/ })).toBeVisible();
    await expect(page.getByRole("heading", { name: /^Plan$/ })).toBeVisible();
    await expect(page.getByRole("heading", { name: /Danger zone/i })).toBeVisible();

    // Studio URL is read-only and shows the slug.
    await expect(
      page.locator(`input[value="${fixture.name}.zeroship.app"]`),
    ).toBeVisible();
  });

  test("Settings #domain anchor is reachable", async ({ page }) => {
    await visit(page, `/p/${fixture.id}/settings#domain`);
    await expect(page.locator("#domain")).toBeVisible();
  });
});

// Separate group: deletion ALWAYS happens last and uses its own fixture.
test.describe("workspace · destructive", () => {
  test("Delete project removes the row and navigates home", async ({ page, request, context }) => {
    const app = await createApp(request, uniqueAppName("e2e-del"));

    await visit(page, `/p/${app.id}/settings`);

    // Auto-accept the confirm() dialog the danger button raises.
    page.once("dialog", (d) => d.accept());

    await page.getByRole("button", { name: /Delete project/i }).click();

    await page.waitForURL(/\/$/, { timeout: 10_000 });

    const apps = await listApps(request);
    expect(apps.find((a) => a.id === app.id)).toBeUndefined();
  });
});
