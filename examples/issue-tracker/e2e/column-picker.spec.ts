import { expect, test } from "@playwright/test";

import { signIn } from "./session";

/**
 * The column picker opens where you can see it.
 *
 * Reported as "the Columns button does not work". It worked -- it toggled
 * state and rendered the menu every time. The menu was a sibling of the whole
 * FilterBar rather than of its button, so `position: absolute; top: 110%`
 * resolved against the nearest positioned ancestor (the page) and 110% of a
 * tall block put it at y=1692 in a 900px viewport: open, visible, and 792px
 * below the fold.
 *
 * So this asserts geometry, not state. `toBeVisible()` passed throughout the
 * bug -- Playwright counts an off-screen element as visible, and a spec that
 * only clicked and checked for the menu would have gone green the whole time.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);

test("the column picker opens within the viewport", async ({ page, baseURL }) => {
  await signIn(page.context(), { runtimePort: RUNTIME_PORT, baseURL: baseURL! });
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto("/bugs");

  const toggle = page.getByRole("button", { name: "Columns" });
  await expect(toggle).toBeVisible();
  await toggle.click();

  const menu = page.locator(".column-picker-menu");
  await expect(menu, "the menu opened").toBeVisible();

  const box = await menu.boundingBox();
  const viewport = page.viewportSize();
  expect(box, "the menu is laid out").toBeTruthy();
  expect(viewport, "the viewport is known").toBeTruthy();
  expect(
    box!.y,
    "the menu starts above the fold rather than a screenful below it",
  ).toBeLessThan(viewport!.height);
  expect(box!.y, "and not off the top either").toBeGreaterThanOrEqual(0);
  expect(
    box!.x + box!.width,
    "and it ends inside the right edge",
  ).toBeLessThanOrEqual(viewport!.width + 1);

  // It also has to DO something: the menu is the only way to change columns.
  const resolution = page.getByRole("columnheader", { name: /Resolution/ });
  await expect(resolution, "Resolution starts visible").toHaveCount(1);
  await menu.getByRole("checkbox", { name: "Resolution" }).click();
  await expect(resolution, "unchecking it removes the column").toHaveCount(0);
});
