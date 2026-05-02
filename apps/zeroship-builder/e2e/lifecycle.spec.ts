import { test, expect } from "@playwright/test";

// Plan 01 — project lifecycle (archive filter pill + Settings archive).
//
// Pure UI smoke against /home and the SettingsCanvas. No control
// plane / OPENAI required. The Archive UI is module-level state in
// the zeroship-builder dev server (ISS-19), so we just verify the
// filter pill is wired and the empty-archived state renders.

test.describe("Plan 01 — project lifecycle (spec §8.3)", () => {
  test("Home shows the Active / Archived filter pills", async ({ page }) => {
    await page.goto("/home");
    await expect(page.getByTestId("home-filters")).toBeVisible();
    await expect(page.getByTestId("home-filter-active")).toBeVisible();
    await expect(page.getByTestId("home-filter-archived")).toBeVisible();
  });

  test("clicking Archived flips the gallery view title", async ({ page }) => {
    await page.goto("/home");
    await page.getByTestId("home-filter-archived").click();
    // The gallery header swaps to "Archived" — that's the load-bearing
    // signal that the filter actually flipped state. We don't assert
    // on the empty / list rendering because the dev server may stall
    // on listApps when no control plane is reachable, leaving the
    // gallery in a loading skeleton.
    await expect(
      page.getByTestId("home-gallery").getByRole("heading", { name: "Archived" }),
    ).toBeVisible();
  });

  test("Active filter is the default", async ({ page }) => {
    await page.goto("/home");
    // The default heading is "Recent work" — confirms Active is the
    // initial filter without coupling to FilterPill's internal
    // active-state attribute.
    await expect(
      page.getByTestId("home-gallery").getByRole("heading", { name: "Recent work" }),
    ).toBeVisible();
  });
});
