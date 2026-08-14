import { expect, test } from "@playwright/test";

/**
 * A person with no session can get one.
 *
 * Deliberately does NOT call the signIn() helper. That helper mints the dev
 * session cookie directly and its own comment says why this spec has to
 * exist: "The login form itself is not covered by these specs." Nothing was,
 * and the gap was not a missing assertion -- it was a missing feature. The
 * app had no way in at all. Every signed-out state said "sign in through the
 * platform and reload" while offering nothing to click, so the only route to
 * an identity was signing a cookie by hand.
 *
 * THREE halves, each of which looked like the whole thing at the time:
 *
 *   1. There was no way in at all -- no button anywhere.
 *   2. The popup mints a cookie but tells the app nothing, so identity had to
 *      be refetched or the header kept reading "Sign in".
 *   3. Refetching identity moved the HEADER and nothing else. Every page runs
 *      its own query at mount; signed out those 401 and render "Sign-in
 *      required", and they went on holding that 401 after a successful sign
 *      in -- still advising a reload, which was the only thing that worked.
 *
 * So this asserts the PAGE changed, not just the chrome. An earlier version
 * checked only that the Sign in button disappeared, and passed through all of
 * (3): the header is the one part that was already working.
 */

test("a signed-out visitor signs in and the app notices", async ({ page, context }) => {
  await page.goto("/dashboard");

  const signIn = page.getByRole("button", { name: "Sign in" });
  await expect(signIn, "the app offers a way in").toBeVisible({ timeout: 15000 });

  const popupPromise = context.waitForEvent("page");
  await signIn.click();
  const popup = await popupPromise;
  await popup.waitForLoadState();

  // The dev tier's form arrives prefilled; a person just confirms.
  const submit = popup.getByRole("button", { name: /sign in|continue/i });
  await expect(submit, "the platform's login form opened").toBeVisible({ timeout: 10000 });
  await submit.click();

  // The session really exists, not merely a UI that looks signed in.
  await expect
    .poll(
      async () => (await context.cookies()).some((c) => c.name === "__zeroship_dev_session"),
      { message: "a dev session cookie was minted", timeout: 15000 },
    )
    .toBe(true);

  // The chrome caught up...
  await expect(
    page.getByRole("button", { name: "Sign in" }),
    "the way-in is gone because there is now an identity",
  ).toHaveCount(0, { timeout: 15000 });

  // ...and so did the PAGE. This is the assertion that matters: the dashboard
  // refuses anonymous access, so if its query still holds the 401 it took at
  // mount, this text is still on screen.
  await expect(
    page.getByText("Sign-in required"),
    "the page ran its query again instead of holding the 401 it took while signed out",
  ).toHaveCount(0, { timeout: 15000 });
  await expect(
    page.getByText(/Assigned to me/),
    "and it renders what a signed-in person came for",
  ).toBeVisible({ timeout: 15000 });
});
