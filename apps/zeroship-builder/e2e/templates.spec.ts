// ─── Templates — gallery, filters, navigation ────────────────────

import { test, expect } from "@playwright/test";
import { visit } from "./helpers";

test.describe("templates", () => {
  test("gallery renders all 12 templates", async ({ page }) => {
    await visit(page, "/templates");
    await expect(page.getByRole("heading", { name: /starting point/i })).toBeVisible();

    const cards = page.locator('[data-testid^="template-card:"]');
    await expect(cards).toHaveCount(12);
  });

  test("category filters narrow the grid", async ({ page }) => {
    await visit(page, "/templates");

    const cards = page.locator('[data-testid^="template-card:"]');
    await expect(cards).toHaveCount(12);

    // sharing → 3 templates (recipe-journal, photo-album, reading-room)
    await page.getByTestId("filter:sharing").click();
    await expect(cards).toHaveCount(3);
    await expect(page.getByTestId("template-card:recipe-journal")).toBeVisible();

    // collecting → 3 (newsletter-signup, booking, survey)
    await page.getByTestId("filter:collecting").click();
    await expect(cards).toHaveCount(3);
    await expect(page.getByTestId("template-card:newsletter-signup")).toBeVisible();

    // selling → 3 (tip-jar, subscription, storefront)
    await page.getByTestId("filter:selling").click();
    await expect(cards).toHaveCount(3);

    // showing → 2 (portfolio, personal-page)
    await page.getByTestId("filter:showing").click();
    await expect(cards).toHaveCount(2);

    // internal → 1 (todo-list)
    await page.getByTestId("filter:internal").click();
    await expect(cards).toHaveCount(1);
    await expect(page.getByTestId("template-card:todo-list")).toBeVisible();

    // back to all
    await page.getByTestId("filter:all").click();
    await expect(cards).toHaveCount(12);
  });

  test("clicking a template card → /new?template=<slug>", async ({ page }) => {
    await visit(page, "/templates");
    await page.getByTestId("template-card:recipe-journal").click();
    await expect(page).toHaveURL(/\/new\?template=recipe-journal/);
    // Wizard should skip step 1 and land on step 2 with the template's
    // default prompt prefilled.
    await expect(page.getByTestId("wiz-step-2")).toBeVisible();
    await expect(
      page.getByTestId("wiz-prompt"),
    ).toHaveValue(/recipe journal/i);
  });

  test("'describe your own' link → /new (blank step 1)", async ({ page }) => {
    await visit(page, "/templates");
    await page.getByTestId("templates-blank").click();
    await expect(page).toHaveURL(/\/new$/);
    await expect(page.getByTestId("wiz-step-1")).toBeVisible();
  });

  test("each template card shows № tag, name, tagline, bullets", async ({ page }) => {
    await visit(page, "/templates");
    const recipe = page.getByTestId("template-card:recipe-journal");
    await expect(recipe).toBeVisible();
    await expect(recipe).toContainText("№ 01");
    await expect(recipe).toContainText("Recipe Journal");
    await expect(recipe).toContainText(/recipes/i);
    await expect(recipe).toContainText(/posts with photos/i);
    await expect(recipe).toContainText(/Use →/i);
  });
});
