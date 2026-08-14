import { expect, test } from "@playwright/test";

import { signIn } from "./session";

/**
 * A signed-in person can sign out.
 *
 * Signing in was added and signing out was not, so the app could take an
 * identity and never put one down: the name in the header was a <span>, and
 * on a shared machine the only way out was clearing the cookie by hand.
 *
 * Asserts the SESSION ends, not just that the header changed. Hiding a name
 * while the cookie still authorises every RPC is the version of this that
 * looks right and is worse than doing nothing.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);

test("signing out ends the session", async ({ page, baseURL, context }) => {
  await signIn(context, { runtimePort: RUNTIME_PORT, baseURL: baseURL! });
  await page.goto("/dashboard");

  // Signed in: the account menu names you.
  const account = page.getByRole("button", { name: /Alice Dev/ });
  await expect(account, "the header names the signed-in person").toBeVisible({ timeout: 15000 });

  await account.click();
  const signOut = page.getByRole("menuitem", { name: "Sign out" });
  await expect(signOut, "the menu offers a way out").toBeVisible();
  await signOut.click();

  // The cookie is really gone.
  await expect
    .poll(
      async () => (await context.cookies()).some((c) => c.name === "__zeroship_dev_session"),
      { message: "the dev session cookie was cleared", timeout: 15000 },
    )
    .toBe(false);

  // And the app noticed, rather than showing its mount-time answer.
  await expect(
    page.getByRole("button", { name: "Sign in" }),
    "the app offers a way back in",
  ).toBeVisible({ timeout: 15000 });

  // The PAGE too, and this is the direction that matters most: a dashboard
  // still showing the previous person's work after they signed out is a
  // privacy problem, not a refresh problem.
  await expect(
    page.getByText("Sign-in required"),
    "the page re-ran its query and now refuses, instead of showing what it fetched for the signed-in user",
  ).toBeVisible({ timeout: 15000 });
});
