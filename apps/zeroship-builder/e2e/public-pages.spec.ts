import { test, expect } from "@playwright/test";

// Plan 01 — public surfaces (per spec §5).
//
// Marketing landing, pricing, skill catalogue, public templates,
// about, changelog, and the legal stubs all render *without* an
// authed session. They depend on no LLM, no control plane, and no
// sandbox controller — these are pure UI smoke tests against the
// vanilla `npm run dev` worktree.

test.describe("Plan 01 — public pre-auth surfaces", () => {
  test("/ renders the marketing landing", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByTestId("marketing-page")).toBeVisible();
    await expect(page.getByTestId("public-nav")).toBeVisible();
    await expect(page.getByTestId("marketing-begin")).toBeVisible();
    await expect(page.getByTestId("marketing-templates")).toBeVisible();
  });

  test("/ → Begin navigates to /new (the wizard)", async ({ page }) => {
    await page.goto("/");
    await page.getByTestId("marketing-begin").click();
    await expect(page).toHaveURL(/\/new$/);
  });

  test("/pricing renders the three plans", async ({ page }) => {
    await page.goto("/pricing");
    await expect(page.getByTestId("pricing-page")).toBeVisible();
    await expect(page.getByTestId("pricing-plans")).toBeVisible();
    await expect(page.getByTestId("pricing-plan-free")).toBeVisible();
    await expect(page.getByTestId("pricing-plan-maker")).toBeVisible();
    await expect(page.getByTestId("pricing-plan-pro")).toBeVisible();
  });

  test("/skills renders the catalogue and filter pills", async ({ page }) => {
    await page.goto("/skills");
    await expect(page.getByTestId("skills-page")).toBeVisible();
    await expect(page.getByTestId("skills-filters")).toBeVisible();
    await expect(page.getByTestId("skills-grid")).toBeVisible();
    // At least one skill card visible from the static seed list.
    await expect(page.getByTestId("skill-card:auth")).toBeVisible();
    await expect(page.getByTestId("skill-card:payments")).toBeVisible();
    // "Add to project" is disabled (registry not wired — ISS-13).
    const addBtn = page.getByTestId("skill-add:auth");
    await expect(addBtn).toBeVisible();
    await expect(addBtn).toBeDisabled();
  });

  test("/templates renders the public gallery", async ({ page }) => {
    await page.goto("/templates");
    await expect(page.getByTestId("templates-page")).toBeVisible();
    await expect(page.getByTestId("templates-filters")).toBeVisible();
    await expect(page.getByTestId("templates-grid")).toBeVisible();
  });

  test("/templates → clicking a template lands on /new", async ({ page }) => {
    await page.goto("/templates");
    // First template card in the grid — id is `template-card:<slug>`.
    const firstCard = page.locator('[data-testid^="template-card:"]').first();
    await expect(firstCard).toBeVisible();
    await firstCard.click();
    await expect(page).toHaveURL(/\/new(\?.*)?$/);
  });

  test("/about renders", async ({ page }) => {
    await page.goto("/about");
    await expect(page.getByTestId("about-page")).toBeVisible();
  });

  test("/changelog renders with at least one entry", async ({ page }) => {
    await page.goto("/changelog");
    await expect(page.getByTestId("changelog-page")).toBeVisible();
    await expect(page.getByTestId("changelog-entries")).toBeVisible();
  });

  test("/legal/privacy renders", async ({ page }) => {
    await page.goto("/legal/privacy");
    await expect(page.getByTestId("privacy-page")).toBeVisible();
  });

  test("/legal/terms renders", async ({ page }) => {
    await page.goto("/legal/terms");
    await expect(page.getByTestId("terms-page")).toBeVisible();
  });

  test("public nav links are present and route correctly", async ({ page }) => {
    await page.goto("/");
    // Pricing link in the nav.
    await page.getByTestId("public-nav-pricing").click();
    await expect(page).toHaveURL(/\/pricing$/);
    await expect(page.getByTestId("pricing-page")).toBeVisible();

    // Templates link.
    await page.getByTestId("public-nav-templates").click();
    await expect(page).toHaveURL(/\/templates$/);
    await expect(page.getByTestId("templates-page")).toBeVisible();

    // Skills link.
    await page.getByTestId("public-nav-skills").click();
    await expect(page).toHaveURL(/\/skills$/);
    await expect(page.getByTestId("skills-page")).toBeVisible();

    // About link.
    await page.getByTestId("public-nav-about").click();
    await expect(page).toHaveURL(/\/about$/);
    await expect(page.getByTestId("about-page")).toBeVisible();
  });
});
