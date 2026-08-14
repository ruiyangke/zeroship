import { expect, test } from "@playwright/test";

import { productKey } from "./keys";
import { signIn } from "./session";

/**
 * Navigation is in the header, and it offers a signed-out visitor only what a
 * signed-out visitor can use.
 *
 * Two changes, one spec, because they are the same claim about the same strip
 * of the page: the four destinations moved out of the collapsible rail into
 * the header band, and the one of them that needs an identity stops being
 * rendered without one. "My dashboard" led to a page whose entire content is a
 * sign-in prompt -- the header already knew better and showed "Sign in" rather
 * than "New issue", and the links did not.
 *
 * Each half is PAIRED with a signed-in control differing in one variable: the
 * session. Without the pair, "no dashboard link" passes against an app that
 * shows nobody any links, and "no Advanced button" passes against one where
 * advanced search was deleted.
 *
 * What is NOT hidden is half the point. Issue browsing, products and reports
 * are anonymous by policy (`src/server/config.ts`: `issues.search`,
 * `products.list`, `products.resolve` and the five `reports.*` are
 * `auth: "anon", publiclyAccessible: true`), so those destinations stay and
 * the assertions below say so.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);
const RUN = `${process.pid}-${Date.now()}`;

test("the primary nav lives in the header band, not in a rail", async ({ page, baseURL }) => {
  await signIn(page.context(), { runtimePort: RUNTIME_PORT, baseURL: baseURL! });
  await page.goto("/issues");

  // Scoped to the banner: this is the whole structural claim. A `nav` anywhere
  // on the page would satisfy an unscoped locator, including the rail this
  // change removed.
  const nav = page.getByRole("banner").getByRole("navigation", { name: "Primary" });
  await expect(nav, "the primary nav is inside the header").toBeVisible();
  for (const label of ["Issues", "My dashboard", "Products", "Reports"]) {
    await expect(nav.getByRole("link", { name: label }), `${label} is reachable`).toBeVisible();
  }

  // The rail and its toggle are gone TOGETHER. The hamburger existed only to
  // collapse the rail, so a build that kept one and dropped the other is a
  // half-applied change, not a working header.
  await expect(
    page.getByRole("button", { name: "Toggle navigation" }),
    "nothing left to toggle",
  ).toHaveCount(0);
  await expect(
    page.locator('[data-slot="app-shell-sidebar"]'),
    "AppShell renders no sidebar rail",
  ).toHaveCount(0);

  // And the links still navigate -- a header full of decoration would pass
  // every assertion above.
  await nav.getByRole("link", { name: "Reports" }).click();
  await expect(page.locator("h1")).toContainText("Reports");
});

test("a signed-out visitor is offered the public destinations and not the private one", async ({
  page,
  baseURL,
  browser,
}) => {
  // Signed IN -- the control half.
  await signIn(page.context(), { runtimePort: RUNTIME_PORT, baseURL: baseURL! });
  await page.goto("/issues");
  const signedInNav = page.getByRole("banner").getByRole("navigation", { name: "Primary" });
  await expect(
    signedInNav.getByRole("link", { name: "My dashboard" }),
    "with a session the dashboard is offered",
  ).toBeVisible();

  // Signed OUT.
  const anon = await browser.newContext();
  const visitor = await anon.newPage();
  await visitor.goto(`${baseURL}/issues`);
  const nav = visitor.getByRole("banner").getByRole("navigation", { name: "Primary" });

  await expect(
    nav.getByRole("link", { name: "My dashboard" }),
    "the dashboard needs an identity, so it is not offered without one",
  ).toHaveCount(0);
  // The paired positive: the public destinations must survive. Hiding these
  // would be a worse bug than the one this fixes -- anonymous browsing is what
  // this tracker is for.
  for (const label of ["Issues", "Products", "Reports"]) {
    await expect(
      nav.getByRole("link", { name: label }),
      `${label} is public and stays`,
    ).toBeVisible();
  }

  await anon.close();
});

test("the issue list keeps the anonymous filters and drops the builder", async ({
  page,
  baseURL,
  browser,
}) => {
  await signIn(page.context(), { runtimePort: RUNTIME_PORT, baseURL: baseURL! });
  await page.goto("/issues");
  await expect(page.locator("table tbody tr").first()).toBeVisible();
  // The control half: advanced search exists for someone who can run it.
  await expect(
    page.getByRole("button", { name: "Advanced..." }),
    "a signed-in user can open the builder",
  ).toBeVisible();

  const anon = await browser.newContext();
  const visitor = await anon.newPage();
  await visitor.goto(`${baseURL}/issues`);

  // The builder runs `search.query` and lists `savedSearches`, both
  // `auth: "user"`, so signed out it opens a dialog that can only fail.
  await expect(
    visitor.getByRole("button", { name: "Advanced..." }),
    "no builder for someone who cannot run a query",
  ).toHaveCount(0);
  // Everything the anonymous `issues.search` serves stays usable. This is the
  // assertion that fails if "hide what does not work" is applied too widely.
  await expect(visitor.locator("table tbody tr").first(), "the list is readable").toBeVisible();
  await expect(visitor.getByRole("button", { name: "Apply" })).toBeVisible();
  await expect(visitor.getByPlaceholder(/^Search, or type/)).toBeVisible();

  await anon.close();
});

test("the products page reads publicly and administers privately", async ({
  page,
  baseURL,
  browser,
}) => {
  await signIn(page.context(), { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const name = `Public products ${RUN}`;
  const res = await page.request.post(`${baseURL}/__zeroship/v1/products.create`, {
    data: { json: { name, key: productKey("PUB"), description: "public list" } },
  });
  expect(res.status(), "products.create should succeed").toBe(200);

  // Signed IN -- the control half. `products.list` is anonymous but everything
  // else on this page (`products.get`, the create/update writers, `groups.list`,
  // `flagTypes.list`) is `auth: "user"`.
  await page.goto("/products");
  await expect(
    page.getByRole("button", { name: "Create product" }),
    "a signed-in user can create a product",
  ).toBeVisible();
  await expect(page.getByRole("heading", { name: "Groups", exact: true }).first()).toBeVisible();

  const anon = await browser.newContext();
  const visitor = await anon.newPage();
  await visitor.goto(`${baseURL}/products`);

  // Public: the list itself.
  await expect(
    visitor.getByText(name),
    "the product list is anonymous, so the names are readable",
  ).toBeVisible();
  await expect(visitor.getByText("You are not signed in")).toBeVisible();

  // Private: everything that writes, and the two admin sections whose list
  // procedures are authenticated -- they used to render as a heading over an
  // error, with a create form underneath.
  await expect(
    visitor.getByRole("button", { name: "Create product" }),
    "no create form without a session",
  ).toHaveCount(0);
  await expect(visitor.getByRole("heading", { name: "Groups", exact: true })).toHaveCount(0);
  await expect(visitor.getByRole("heading", { name: "Flag types" })).toHaveCount(0);
  // The product names are text, not buttons: pressing one opened the editor,
  // whose first act is the authenticated `products.get`.
  await expect(
    visitor.getByRole("button", { name: new RegExp(`Public products ${RUN}`) }),
    "a name a visitor cannot edit is not a button",
  ).toHaveCount(0);

  await anon.close();
});

test("the reports page is public in full", async ({ browser, baseURL }) => {
  // No signed-in half: this one is a pure negative about hiding, and the
  // signed-in reports page is covered by unknown-route.spec.ts and
  // reports-labels.spec.ts. All five `reports.*` procedures are anonymous, so
  // nothing here should have been touched by the sweep -- which is exactly the
  // regression worth pinning, since "hide what a visitor cannot use" applied
  // one page too far lands here.
  const anon = await browser.newContext();
  const visitor = await anon.newPage();
  await visitor.goto(`${baseURL}/reports`);

  await expect(visitor.locator("h1")).toContainText("Reports");
  await expect(visitor.getByRole("heading", { name: "Summary" })).toBeVisible();
  await expect(visitor.getByText(/sign-in required/i)).toHaveCount(0);

  await anon.close();
});
