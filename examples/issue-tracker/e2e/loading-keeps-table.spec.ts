import { expect, test } from "@playwright/test";

import { signIn } from "./session";

/**
 * The first load marks the table as loading; it does not replace it.
 *
 * Swapping the whole surface for a spinner throws away the column headers and
 * the page height, so the layout jumps when rows land and you lose the thing
 * you were looking at. The refresh path already kept the table -- the FIRST
 * load did not, because the table's own `loading` flag can only apply once
 * there is data to keep, and the wrapper short-circuited to a spinner before
 * the table ever mounted.
 *
 * Held open by delaying the search response, because the real thing is over in
 * milliseconds against a local runtime. The delay is on the ROUTE rather than
 * a fixed wait in the test, so this asserts the loading state itself rather
 * than whatever happens to be on screen after a sleep.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);

test("the bug table is marked loading rather than hidden on first load", async ({
  page,
  baseURL,
}) => {
  await signIn(page.context(), { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  // Queries go out as GET with a base64 `input` query param, not POST -- a
  // pattern match on the method here would never fire.
  let released: (() => void) | null = null;
  const held = new Promise<void>((resolve) => {
    released = resolve;
  });
  await page.route(/bugs\.search/, async (route) => {
    await held;
    await route.continue();
  });

  await page.goto("/bugs");

  // While the search is still in flight: the table is there, marked busy.
  const table = page.locator("table");
  await expect(table, "the table renders while the first load is in flight").toBeVisible();
  await expect(
    page.getByText("Loading bugs..."),
    "and it is not replaced by a bare spinner",
  ).toHaveCount(0);
  await expect(
    page.locator("th", { hasText: "Status" }),
    "the column headers survive the load, so the layout does not jump",
  ).toBeVisible();

  released!();

  // And the real rows arrive into the same table.
  await expect(page.locator("tbody tr").first()).toBeVisible({ timeout: 20_000 });
});
