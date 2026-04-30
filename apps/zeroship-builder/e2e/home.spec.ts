// ─── Home — hero prompt, suggestions, project gallery ───────────

import { test, expect } from "@playwright/test";
import { listApps, visit } from "./helpers";

test.describe("home", () => {
  test("hero renders with prompt + Begin button", async ({ page }) => {
    await visit(page, "/");

    await expect(
      page.getByRole("heading", { name: /What will you/i }),
    ).toBeVisible();

    const prompt = page.getByTestId("home-prompt");
    await expect(prompt).toBeVisible();
    await expect(prompt).toHaveAttribute("placeholder", /recipe sharing/i);

    // Begin is disabled until something is typed
    const submit = page.getByTestId("home-submit");
    await expect(submit).toBeDisabled();
  });

  test("suggestion chip fills the textarea", async ({ page }) => {
    await visit(page, "/");

    const chip = page.getByRole("button", {
      name: /tip calculator that splits unevenly/i,
    });
    await expect(chip).toBeVisible();
    await chip.click();

    const prompt = page.getByTestId("home-prompt");
    await expect(prompt).toHaveValue(/tip calculator/);

    // Begin is now enabled
    await expect(page.getByTestId("home-submit")).toBeEnabled();
  });

  test("submitting prompt navigates to /new with the text in the URL", async ({ page }) => {
    await visit(page, "/");

    const prompt = page.getByTestId("home-prompt");
    await prompt.fill("a tiny pet daycare booking page");
    await page.getByTestId("home-submit").click();

    // Wizard lands on step 1 (no template chosen) — but the URL carries the
    // prompt. Clicking "Or describe your own" preserves it into step 2.
    await expect(page).toHaveURL(/\/new\?prompt=/);
    expect(page.url()).toMatch(/prompt=a%20tiny%20pet%20daycare/i);
    await expect(page.getByTestId("wiz-step-1")).toBeVisible();

    await page.getByTestId("wiz-blank").click();
    await expect(page.getByTestId("wiz-step-2")).toBeVisible();
    await expect(page.getByTestId("wiz-prompt")).toHaveValue(/tiny pet daycare/i);
  });

  test("Cmd+Enter on the textarea also submits", async ({ page }) => {
    await visit(page, "/");

    const prompt = page.getByTestId("home-prompt");
    await prompt.fill("a knitting circle scheduler");
    await prompt.press("Meta+Enter");
    await expect(page).toHaveURL(/\/new\?prompt=/);
  });

  test("project gallery reflects backend state", async ({ page, request }) => {
    const apps = await listApps(request);

    await visit(page, "/");
    await expect(page.getByTestId("home-gallery")).toBeVisible();

    if (apps.length === 0) {
      await expect(page.getByText(/No projects yet/i)).toBeVisible();
    } else {
      // The card for the first (most recently updated) app should appear.
      const sorted = [...apps].sort(
        (a, b) => (b.updated_at ?? "").localeCompare(a.updated_at ?? ""),
      );
      const first = sorted[0];
      const card = page.getByTestId(`project-card:${first.name}`);
      await expect(card).toBeVisible();
    }
  });

  test("project count meta matches listApps()", async ({ page, request }) => {
    const apps = await listApps(request);
    await visit(page, "/");
    await expect(page.getByTestId("home-gallery")).toBeVisible();
    await expect(
      page.getByTestId("home-gallery").getByText(`${apps.length} projects`),
    ).toBeVisible();
  });

  test("clicking a project card navigates to the workspace", async ({ page, request }) => {
    const apps = await listApps(request);
    test.skip(apps.length === 0, "no app to click");
    const target = apps[0];

    await visit(page, "/");
    await page.getByTestId(`project-card:${target.name}`).first().click();
    await expect(page).toHaveURL(new RegExp(`/p/${target.id}/preview$`));
  });

  test('Browse templates link → /templates', async ({ page }) => {
    await visit(page, "/");
    await page.getByRole("link", { name: /Browse templates/i }).click();
    await expect(page).toHaveURL(/\/templates$/);
  });
});
