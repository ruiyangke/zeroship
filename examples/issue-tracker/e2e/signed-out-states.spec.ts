import { expect, test } from "@playwright/test";

/**
 * A signed-out visitor is told to sign in, never shown an empty account.
 *
 * The dashboard asks "which issues are assigned to ME", and with no identity it
 * fell back to `Promise.resolve([])` and rendered "Assigned to me (0) --
 * Nothing here". That is a statement about the visitor's issues, and it is not
 * true: we do not know who they are.
 *
 * Nothing downstream could tell the two apart, which is why this survived.
 * `searchIssues` is anonymous and answers an empty list rather than a 401, so
 * "no issues" and "no identity" arrive as the same value. Only the page knows
 * the difference, so the page has to make it.
 *
 * No `signIn` call anywhere in this file -- that absence is the fixture.
 */

test("the dashboard asks a signed-out visitor to sign in instead of reporting zero", async ({
  page,
}) => {
  await page.goto("/dashboard");

  await expect(page.getByText(/sign-in required/i), "the page states the real reason").toBeVisible();

  // The counted sections must not appear at all. Rendering them as "(0)"
  // alongside the sign-in prompt is worse than either alone: it answers the
  // question and disclaims the answer in the same view.
  await expect(
    page.getByText(/assigned to me/i),
    "an account section must not report a count without an account",
  ).toHaveCount(0);
  await expect(page.getByText(/reported by me/i)).toHaveCount(0);
  await expect(page.getByText(/nothing here/i)).toHaveCount(0);
});

test("the issue list stays public and readable while signed out", async ({ page }) => {
  // The paired half. The fix must not turn every page into a sign-in wall:
  // browsing issues is deliberately anonymous, and a spec that only checked the
  // dashboard would pass on an app that had locked the whole tracker.
  await page.goto("/issues");
  await expect(page.locator("table")).toBeVisible();
  await expect(page.getByText(/sign-in required/i)).toHaveCount(0);
});
