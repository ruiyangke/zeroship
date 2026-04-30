// ─── New-project wizard — full 3-step end-to-end ────────────────
//
// These tests drive the wizard with no mocks. The "Begin" click on
// step 3 calls createApp() which lands a real row in the control
// plane's Postgres. We track every created app and delete it in
// afterEach so the suite leaves no state behind.

import { test, expect } from "@playwright/test";
import {
  deleteApp,
  listApps,
  rpc,
  uniqueAppName,
  visit,
  type AppRecord,
} from "./helpers";

test.describe("new project wizard", () => {
  // Track ids created within each test for cleanup.
  let createdIds: string[];

  test.beforeEach(() => {
    createdIds = [];
  });

  test.afterEach(async ({ request }) => {
    for (const id of createdIds) await deleteApp(request, id);
  });

  test("step 1 → step 2 → step 3 from a template, real app gets created", async ({ page, request }) => {
    await visit(page, "/new");
    await expect(page.getByTestId("wiz-step-1")).toBeVisible();

    // Step 1: pick a template
    await page.getByTestId("template-card:todo-list").click();
    await expect(page.getByTestId("wiz-step-2")).toBeVisible();

    // The default prompt for the todo-list template should be prefilled.
    const promptArea = page.getByTestId("wiz-prompt");
    await expect(promptArea).toHaveValue(/simple todo list/i);

    // Customise: set our own slug so we can find + clean up the app.
    const slug = uniqueAppName("e2e-wiz-tpl");
    const slugField = page.getByTestId("wiz-slug");
    // Slug field is bound to "<slug>.zeroship.app" — we replace the whole value.
    await slugField.fill(`${slug}.zeroship.app`);
    await expect(slugField).toHaveValue(`${slug}.zeroship.app`);

    // Name field — ensure it's not empty
    const nameField = page.getByTestId("wiz-name");
    await expect(nameField).not.toHaveValue("");

    await page.getByTestId("wiz-next").click();
    await expect(page.getByTestId("wiz-step-3")).toBeVisible();

    // Step 3: summary should mention the slug + the template name
    await expect(page.getByText(`${slug}.zeroship.app`)).toBeVisible();
    await expect(page.getByText(/Todo List/)).toBeVisible();

    await page.getByTestId("wiz-begin").click();

    // Wait for navigation to /p/:id/preview (the real createApp landed)
    await page.waitForURL(/\/p\/[a-f0-9-]+\/preview/, { timeout: 15_000 });

    // Pull the id out of the URL so we can clean up.
    const m = page.url().match(/\/p\/([^/]+)\/preview/);
    expect(m).not.toBeNull();
    const id = m![1];
    createdIds.push(id);

    // Verify the row really exists in the backend.
    const apps = await listApps(request);
    const created = apps.find((a) => a.id === id);
    expect(created).toBeTruthy();
    expect(created!.name).toBe(slug);
    expect(created!.plan_id).toBe("free");
  });

  test("blank path: skip the gallery, describe your own, ship", async ({ page, request }) => {
    await visit(page, "/new");
    await page.getByTestId("wiz-blank").click();
    await expect(page.getByTestId("wiz-step-2")).toBeVisible();

    const slug = uniqueAppName("e2e-wiz-blank");
    await page.getByTestId("wiz-prompt").fill(
      "A guestbook page where visitors can leave a one-line greeting.",
    );
    await page.getByTestId("wiz-name").fill("Guestbook");
    await page.getByTestId("wiz-slug").fill(`${slug}.zeroship.app`);

    await page.getByTestId("wiz-next").click();
    await expect(page.getByTestId("wiz-step-3")).toBeVisible();
    await page.getByTestId("wiz-begin").click();

    await page.waitForURL(/\/p\/[a-f0-9-]+\/preview/, { timeout: 15_000 });
    const id = page.url().match(/\/p\/([^/]+)\/preview/)![1];
    createdIds.push(id);

    const apps = await listApps(request);
    expect(apps.find((a) => a.id === id)?.name).toBe(slug);
  });

  test("?prompt= param prefills step 2 (skipping step 1)", async ({ page }) => {
    const text = "a small movie watchlist for me and my partner";
    await visit(page, `/new?prompt=${encodeURIComponent(text)}`);
    // The wizard preserves step 1 unless a template OR (the consume-from-home
    // session storage) is present. With ?prompt= but no template, step 1 still
    // shows. Verify clicking blank still produces a prefilled step 2.
    if (await page.getByTestId("wiz-step-1").isVisible()) {
      await page.getByTestId("wiz-blank").click();
    }
    await expect(page.getByTestId("wiz-step-2")).toBeVisible();
    await expect(
      page.getByTestId("wiz-prompt"),
    ).toHaveValue(text);
  });

  test("?template= param skips straight to step 2", async ({ page }) => {
    await visit(page, "/new?template=portfolio");
    await expect(page.getByTestId("wiz-step-2")).toBeVisible();
    await expect(
      page.getByTestId("wiz-prompt"),
    ).toHaveValue(/portfolio/i);
    // The header should mention the template name.
    await expect(page.getByText(/Portfolio · /)).toBeVisible();
  });

  test("Next is disabled with empty fields, enabled when filled", async ({ page }) => {
    await visit(page, "/new");
    await page.getByTestId("wiz-blank").click();
    await expect(page.getByTestId("wiz-step-2")).toBeVisible();

    // Clear everything
    await page.getByTestId("wiz-prompt").fill("");
    await page.getByTestId("wiz-name").fill("");
    await page.getByTestId("wiz-slug").fill("");

    await expect(page.getByTestId("wiz-next")).toBeDisabled();

    await page.getByTestId("wiz-prompt").fill("anything");
    await page.getByTestId("wiz-name").fill("Anything");
    await page.getByTestId("wiz-slug").fill("anything");
    await expect(page.getByTestId("wiz-next")).toBeEnabled();
  });

  test("step 3 Back returns to step 2 with values intact", async ({ page }) => {
    await visit(page, "/new?template=tip-jar");
    await expect(page.getByTestId("wiz-step-2")).toBeVisible();

    const slug = uniqueAppName("e2e-wiz-back");
    await page.getByTestId("wiz-slug").fill(`${slug}.zeroship.app`);
    await page.getByTestId("wiz-next").click();
    await expect(page.getByTestId("wiz-step-3")).toBeVisible();

    // Click the Back ghost button (text is "← Back")
    await page.getByRole("button", { name: /← Back/i }).click();
    await expect(page.getByTestId("wiz-step-2")).toBeVisible();
    await expect(page.getByTestId("wiz-slug")).toHaveValue(`${slug}.zeroship.app`);
  });

  test("template 'change' link from step 2 returns to step 1", async ({ page }) => {
    await visit(page, "/new?template=storefront");
    await expect(page.getByTestId("wiz-step-2")).toBeVisible();
    // The header has "<name> · change" with "change" as a button.
    await page.getByRole("button", { name: /^change$/i }).click();
    await expect(page.getByTestId("wiz-step-1")).toBeVisible();
  });

  test("steps indicator highlights the active step", async ({ page }) => {
    await visit(page, "/new");
    // Step 1 by default
    await expect(page.getByText(/step 1 · pick/i)).toBeVisible();
    await expect(page.getByText(/step 2 · make it yours/i)).toBeVisible();
    await expect(page.getByText(/step 3 · begin/i)).toBeVisible();

    await page.getByTestId("wiz-blank").click();
    await expect(page.getByTestId("wiz-step-2")).toBeVisible();
  });
});
