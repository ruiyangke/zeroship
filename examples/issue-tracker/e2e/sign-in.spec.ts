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
 * Two halves, and the second is the one that made it look broken even after
 * the button existed: the popup mints the cookie, but identity here comes
 * from users.me fetched once at mount, so without a refetch the flow would
 * succeed and the header would still read "Sign in".
 */

test("a signed-out visitor signs in and the app notices", async ({ page, context }) => {
  await page.goto("/#/bugs");

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

  // And the app asked the server again rather than showing its mount-time answer.
  await expect(
    page.getByRole("button", { name: "Sign in" }),
    "the way-in is gone because there is now an identity",
  ).toHaveCount(0, { timeout: 15000 });
});
