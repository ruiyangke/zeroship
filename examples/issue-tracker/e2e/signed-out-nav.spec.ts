import { expect, test } from "@playwright/test";

import { productKey } from "./keys";
import { openProductsAdmin } from "./more";
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
    page.locator('[data-slot~="app-shell-sidebar"]'),
    "AppShell renders no sidebar rail",
  ).toHaveCount(0);

  // Root renders the same issue list as /issues, so its navigation state must
  // make the same claim. Otherwise identical screens disagree about where the
  // reader is solely because one arrived through the shorter URL.
  await page.goto("/");
  await expect(
    nav.getByRole("link", { name: "Issues" }),
    "the root issue list marks Issues as the current destination",
  ).toHaveAttribute("aria-current", "page");

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
  // Creating is a dialog behind this button now, not a form above the list.
  // Asserted THROUGH to the form: a trigger that opens nothing would satisfy a
  // check on the button alone, and the claim here is that a signed-in user can
  // actually create a product.
  await page.getByRole("button", { name: "New product" }).click();
  const createProduct = page.getByRole("button", { name: "Create product" });
  const cancel = page.getByRole("button", { name: "Cancel" });
  await expect(
    createProduct,
    "a signed-in user can reach the create form",
  ).toBeVisible();
  await expect(createProduct, "the dialog primary action keeps medium emphasis").toHaveCSS(
    "height",
    "28px",
  );
  await expect(cancel, "the paired dialog action stays the same height").toHaveCSS(
    "height",
    "28px",
  );

  // The old active colour dropped the white label below 4.5:1 and made the
  // primary action resemble a disabled control. Hold the real :active state
  // past the theme's transition, then check contrast and geometry together.
  await page.getByRole("textbox", { name: "Name" }).fill("Contrast probe");
  const relativeBox = () =>
    createProduct.evaluate((element) => {
      const box = element.getBoundingClientRect();
      const parent = element.parentElement?.getBoundingClientRect();
      if (!parent) throw new Error("the dialog action has no parent box");
      return {
        x: box.x - parent.x,
        y: box.y - parent.y,
        width: box.width,
        height: box.height,
      };
    });
  const restBox = await relativeBox();
  await createProduct.hover();
  await page.waitForTimeout(120);
  await page.mouse.down();
  await page.waitForTimeout(120);
  const active = await createProduct.evaluate((element) => {
    const parse = (value: string) =>
      value.match(/[\d.]+/g)?.slice(0, 3).map(Number) ?? [0, 0, 0];
    const luminance = (rgb: number[]) => {
      const channels = rgb.map((channel) => {
        const value = channel / 255;
        return value <= 0.04045 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4;
      });
      return 0.2126 * channels[0] + 0.7152 * channels[1] + 0.0722 * channels[2];
    };
    const style = getComputedStyle(element);
    const foreground = luminance(parse(style.color));
    const background = luminance(parse(style.backgroundColor));
    return (Math.max(foreground, background) + 0.05) /
      (Math.min(foreground, background) + 0.05);
  });
  const activeBox = await relativeBox();
  expect(active, "the active filled action keeps readable text contrast").toBeGreaterThanOrEqual(4.5);
  expect(activeBox, "hover and active states do not move the control").toEqual(restBox);
  await page.mouse.move(0, 0);
  await page.mouse.up();

  await cancel.click();

  await page.getByRole("searchbox").fill(name);
  const openCount = page.getByText("0 open", { exact: true }).locator("..");
  await expect(openCount, "the product total registers above component chips").toHaveCSS(
    "height",
    "28px",
  );
  // Groups and flag types are the second tab -- see openProductsAdmin.
  await openProductsAdmin(page);
  await expect(page.getByRole("heading", { name: "Groups", exact: true }).first()).toBeVisible();

  const anon = await browser.newContext();
  const visitor = await anon.newPage();
  await visitor.goto(`${baseURL}/products`);

  // Public: the list itself. Searched for rather than scrolled to -- the list
  // is paged at 25 and this fixture sorts under P in a database of 200-odd
  // products, so it is on page seven. Which also exercises the search a
  // visitor is offered.
  await visitor.getByRole("searchbox").fill(name);
  await expect(
    visitor.getByText(name),
    "the product list is anonymous, so the names are readable",
  ).toBeVisible();
  await expect(visitor.getByText("You are not signed in")).toBeVisible();

  // Private: everything that writes, and the two admin sections whose list
  // procedures are authenticated -- they used to render as a heading over an
  // error, with a create form underneath.
  await expect(
    visitor.getByRole("button", { name: "New product" }),
    "no create form without a session",
  ).toHaveCount(0);
  await expect(
    visitor.getByRole("tab", { name: "Administration" }),
    "and no tab leading to one",
  ).toHaveCount(0);
  await expect(visitor.getByRole("heading", { name: "Groups", exact: true })).toHaveCount(0);
  await expect(visitor.getByRole("heading", { name: "Flag types" })).toHaveCount(0);
  // The product names are text, not buttons: pressing one opens the editor,
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

/**
 * A column whose content is authenticated is not shown to a visitor.
 *
 * `issues.search` is anonymous but `users.resolve` is not, so a signed-out
 * reader gets every row and no names: Assignee and Reporter rendered a full
 * column of "--". `useIssueLookups` documents that fallback as intended
 * degradation, and for a single cell it is -- a deleted user still leaves an
 * id worth showing. A whole column of it answers nothing and was taking width
 * from Summary.
 *
 * The control is the point here. Asserting only that a visitor lacks the
 * column would pass just as well if the column had been deleted outright, so
 * the same page is read with a session and must still have it.
 *
 * WHAT THIS DOES NOT CATCH: it reads the DEFAULT column set. A reader who had
 * previously enabled Reporter has it in localStorage; this spec clears that
 * first, so it says nothing about the stored-preference path.
 */
test("columns that need an identity are absent for a visitor and present with one", async ({
  browser,
  baseURL,
}) => {
  const headersFor = async (context: Awaited<ReturnType<typeof browser.newContext>>) => {
    const page = await context.newPage();
    // Clear first: a stored column choice would decide this instead of the
    // session, and the failure would look like the gate not working.
    await page.goto("/issues");
    await page.evaluate(() => window.localStorage.clear());
    await page.goto("/issues");
    await page.locator("table thead th").first().waitFor();
    // WAIT FOR THE SESSION, not for the table. These are different moments and
    // this spec used to conflate them: the header renders long before
    // `users.me` answers, and the identity columns are deliberately absent
    // during that window, so reading here caught the signed-in run mid-flight
    // and saw no Assignee. It failed 3 times in 6 runs.
    //
    // The header is the honest signal because it is the one component that
    // always modelled the third state -- `UserChip` shows this exact string
    // while the request is open, then swaps to Sign in or the account menu.
    await expect(
      page.getByText("checking session..."),
      "the session resolved, so the columns below reflect an answer rather than the wait",
    ).toHaveCount(0);
    return page.locator("table thead th").allInnerTexts();
  };

  const visitor = await browser.newContext({ baseURL });
  const anonymous = await headersFor(visitor);
  await visitor.close();

  const member = await browser.newContext({ baseURL });
  await signIn(member, { runtimePort: RUNTIME_PORT, baseURL: baseURL! });
  const authenticated = await headersFor(member);
  await member.close();

  expect(
    anonymous.map((h) => h.trim()),
    "a visitor is not offered a column that can only render dashes",
  ).not.toContain("Assignee");
  expect(
    authenticated.map((h) => h.trim()),
    "and the column is genuinely still there with a session -- otherwise this passes against a deleted column",
  ).toContain("Assignee");

  // Everything else is identical, so the gate is about identity and not a
  // second, accidental difference between the two views.
  const withoutPeople = authenticated.filter((h) => !["Assignee", "Reporter"].includes(h.trim()));
  expect(
    anonymous.map((h) => h.trim()),
    "the two views differ ONLY by the identity columns",
  ).toEqual(withoutPeople.map((h) => h.trim()));
});
