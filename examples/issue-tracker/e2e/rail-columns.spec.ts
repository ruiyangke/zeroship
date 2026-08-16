import { expect, test } from "@playwright/test";

import { signIn } from "./session";

/**
 * Every rail row starts its value in the SAME column.
 *
 * The rail renders five row kinds and each used to decide its own columns:
 * `.rail-choice` a fixed 7.5rem triple, `DescriptionList` a max-content pair,
 * `.field-row-compact` a flexbox whose label was as wide as its own text,
 * `.field-block` no columns at all, and `RailDisclosure` wrapping a
 * `.rail-choice` one level deeper than the others. So reading down one 22rem
 * column the eye crossed three different value positions and one control that
 * belonged to no column.
 *
 * The fix is that `.rail-fields` owns `grid-template-columns` and every row
 * kind spans it with `grid-template-columns: subgrid`.
 *
 * The DISCLOSURE row is the one this test exists for. The other four were
 * measured when the change was made; the disclosure rows were not, and they
 * are the ones most likely to regress, because their `.rail-choice` is a
 * GRANDCHILD of the grid. Subgrid only passes tracks to direct children, so a
 * wrapper that stops being a grid item -- someone adding `display: contents`
 * to `.rail-disclosure`, or a rule that resets its `grid-column` -- silently
 * drops these five rows back to a single column while the other four stay
 * aligned and the page still looks half right.
 *
 * What this does NOT catch: it reads one row of each kind, so a rail where
 * only the SECOND Version row is off would pass. It also says nothing about
 * vertical rhythm, which is the other half of the column looking right.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);

test("every rail row kind starts its value in the same column", async ({
  page,
  context,
  baseURL,
}) => {
  await signIn(context, { runtimePort: RUNTIME_PORT, baseURL: baseURL! });
  await page.setViewportSize({ width: 1440, height: 900 });

  await page.goto("/issues");
  await page.locator("a.issue-link").first().click();
  // Wait for CONTENT. `networkidle` fires while react-query is still
  // resolving, and a measurement taken against the loading skeleton reports
  // the positions of elements that are about to be replaced.
  await expect(page.getByText(/loading issue/i)).toHaveCount(0, { timeout: 30_000 });
  // `.rail-section` is a <p>, NOT a heading -- an earlier version of this test
  // waited on getByRole("heading") and timed out against a fully rendered
  // page, reporting a broken rail when the only broken thing was the locator.
  await expect(page.locator(".rail-section", { hasText: "Classification" })).toBeVisible();

  const left = async (locator: ReturnType<typeof page.locator>, what: string) => {
    // scrollIntoViewIfNeeded, not toBeVisible: the disclosure rows sit below
    // the fold at 900px tall, and an unscrolled boundingBox is still the real
    // layout x -- but scrolling first keeps a failure screenshot useful.
    await expect(locator, `${what} must be present to be measured`).toHaveCount(1);
    await locator.scrollIntoViewIfNeeded();
    const box = await locator.boundingBox();
    expect(box, `${what} has no box`).not.toBeNull();
    return Math.round(box!.x);
  };

  // One row of each kind. Severity is a RailChoice, Product a DescriptionList
  // detail, CC a RailDisclosure summary.
  const severity = await left(
    page.locator('.rail-choice', { has: page.getByText("Severity", { exact: true }) })
      .locator(".rail-choice-value"),
    "the Severity value (RailChoice)",
  );
  const product = await left(
    page.locator('[data-slot~="description-list-item"]', {
      has: page.getByText("Product", { exact: true }),
    }).locator('[data-slot~="description-list-detail"]'),
    "the Product detail (DescriptionList)",
  );
  const cc = await left(
    page.locator(".rail-disclosure", { has: page.getByText("CC", { exact: true }) })
      .locator(".rail-choice-value"),
    "the CC summary (RailDisclosure)",
  );

  // Equality, not a tolerance. These share one set of grid tracks, so they are
  // either the same number or the subgrid chain is broken somewhere. A
  // tolerance here would pass the exact regression the test is for: an inner
  // grid falling back to its own columns lands a few pixels off, not far off.
  expect(
    { severity, product, cc },
    "all three sit on the rail's value track",
  ).toEqual({ severity, product: severity, cc: severity });
});
