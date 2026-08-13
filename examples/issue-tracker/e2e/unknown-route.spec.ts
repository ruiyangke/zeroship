import { expect, test } from "@playwright/test";

import { signIn } from "./session";

/**
 * A hash that matches nothing says so, instead of rendering the bug list.
 *
 * The router's default arm returned `{ name: "bugs" }`, so `#/reprots`, a
 * stale bookmark, or a link with a dropped segment all answered with a
 * plausible, fully populated page. That is the worst shape of wrong: nothing
 * looks broken, so you conclude the data is missing rather than the URL.
 *
 * The pair below differs in ONE variable -- whether the hash is a real route.
 * The unknown case alone would pass against an app that showed "No page here"
 * for everything.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);

test("an unknown hash reports itself rather than impersonating the bug list", async ({
  page,
  baseURL,
}) => {
  await signIn(page.context(), { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  // The control: a real route still renders its page.
  await page.goto("/#/reports");
  await expect(page.locator("h1"), "a known route renders its own page").toContainText("Reports");

  // The regression: a hash that matches nothing.
  await page.goto("/#/reprots");
  await expect(
    page.getByText("No page here"),
    "an unknown hash is named as unknown",
  ).toBeVisible();
  await expect(
    page.locator("code"),
    "and it shows which path failed, so a typo is distinguishable from a bug",
  ).toContainText("reprots");
  await expect(
    page.locator("table"),
    "the bug list is NOT rendered in its place",
  ).toHaveCount(0);

  // The way out works.
  await page.getByRole("link", { name: "bug list" }).click();
  await expect(page.locator("h1")).toContainText("Bugs");
});
