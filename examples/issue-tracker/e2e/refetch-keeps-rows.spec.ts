import { expect, test } from "@playwright/test";

import { signIn } from "./session";

/**
 * Reloading a list keeps the list on screen.
 *
 * `useAsync` used to set `status: "loading"` on every reload, which threw away
 * the data it already had, and `AsyncSection` unmounted its children for the
 * duration. So changing a filter, sorting, turning a page or doing anything
 * that reloads blanked the section and jumped the layout, then filled it back
 * in -- for a query that in most cases returned nearly the same rows.
 *
 * Stale rows are not wrong, they are just old. They stay, dimmed and marked
 * `aria-busy`, until the new ones arrive.
 *
 * The route is delayed deliberately. Without that the refetch completes
 * between two Playwright calls and the test passes without ever observing the
 * state it exists to check -- green because it was too slow to look.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);

test("the bug table stays on screen and marks itself busy while reloading", async ({
  page,
  baseURL,
  context,
}) => {
  await signIn(context, { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  await page.goto("/#/bugs");
  const rows = page.locator("table tbody tr");
  // Wait for REAL rows, not the skeleton. The first load now renders the table
  // in its loading state instead of a spinner, so "a tbody tr is visible" is
  // satisfied by placeholder rows -- this sampled five of those as `before`
  // and then compared them against twenty-five real ones. A bug link only
  // exists on a row with data behind it.
  await expect(page.locator("a.bug-link").first()).toBeVisible();
  const before = await rows.count();
  expect(before, "the fixture needs rows for there to be anything to keep").toBeGreaterThan(0);

  // The trailing glob is load-bearing. Queries go out as GET with a base64
  // `input` query string, so "**/bugs.search" matches nothing and the
  // delay never applies -- the refetch then completes between two calls and
  // the test passes without ever observing the state it exists to check.
  await page.route("**/__zeroship/v1/bugs.search*", async (route) => {
    await new Promise((resolve) => setTimeout(resolve, 1500));
    await route.continue();
  });

  // Sorting reloads from the server. Any reload would do; this one is a click
  // a person actually makes.
  await page.getByRole("button", { name: /^Priority/ }).click();

  // Sampled DURING the refetch, not after.
  await expect(page.locator("[aria-busy='true']"), "the table marks itself busy").toHaveCount(1);
  expect(
    await rows.count(),
    "the rows must still be there -- a refetch is not a reason to empty the table",
  ).toBe(before);
  await expect(page.locator("table")).toHaveCount(1);

  // And it clears once the data lands, rather than staying dimmed forever.
  await expect(page.locator("[aria-busy='true']")).toHaveCount(0, { timeout: 10_000 });
  await expect(rows.first()).toBeVisible();
});
