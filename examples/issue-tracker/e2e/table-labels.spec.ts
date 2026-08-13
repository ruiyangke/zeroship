import { expect, test } from "@playwright/test";

import { signIn } from "./session";

/**
 * Every bug table names its product and assignee.
 *
 * `BugResultsTable` takes `productsById` and `usersById` and falls back to the
 * raw id for anything missing. Those props were optional and three of the four
 * call sites -- the dashboard and both advanced-search tables -- left them out
 * entirely, so those pages printed `prod_034607nk...` and `user_0345pl8p...`
 * in the columns a reader scans. Only the bug list passed them, which is why
 * looking at one page made the table seem fine.
 *
 * The props are required now, so a NEW call site cannot repeat this. This spec
 * covers the other half the compiler cannot see: that the maps a caller passes
 * actually arrive populated, over the real fetches, in the browser.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);

const RUN = `${process.pid}-${Date.now()}`;

// Deliberately loose about the id body, so a change to the id scheme does not
// quietly stop this from matching.
const RAW_ID = /\b(prod|user)_[A-Za-z0-9]{10,}/;

test("no bug table falls back to raw product or user ids", async ({ page, baseURL, context }) => {
  await signIn(context, { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const productName = `Tables ${RUN}`;
  const product = await rpc("products.create", { name: productName, description: "tables" });
  const component = await rpc("components.create", {
    productId: product.id,
    name: "Core",
    description: "core",
  });
  const version = await rpc("versions.create", { productId: product.id, name: "1.0" });
  const bug = await rpc("bugs.create", {
    productId: product.id,
    componentId: component.id,
    versionId: version.id,
    summary: `Table row ${RUN}`,
    description: "row",
  });
  const me = await rpc("users.me", {});
  // Assigned and CC'd so the row reaches the dashboard's sections, which are
  // the ones that were wrong.
  await rpc("bugs.reassign", { id: bug.id, assigneeId: me.id });
  await rpc("cc.add", { bugId: bug.id, userId: me.id });

  const tableOf = (page_: typeof page) => page_.locator("table").first();

  // The bug list, filtered to this run so the assertion is about this row.
  await page.goto("/#/bugs");
  await page.getByPlaceholder("Search summary").fill(RUN);
  await page.getByRole("button", { name: "Apply" }).click();
  await expect(tableOf(page)).toContainText(productName);
  expect(
    ((await tableOf(page).textContent()) ?? "").match(RAW_ID)?.[0] ?? null,
    "the bug list should not print raw ids",
  ).toBeNull();

  // The dashboard. Its three tables are the ones that were rendering ids, and
  // they are unfiltered, so this checks every row rather than just ours --
  // a stronger claim, and the rows all come from the same dev database.
  await page.goto("/#/dashboard");
  const sections = page.locator("section.dashboard-section");
  await expect(sections.first()).toBeVisible();
  // Wait for the lookups to arrive; before they do, the table legitimately
  // shows ids and asserting immediately would be a race rather than a check.
  await expect(page.locator("table").first()).toContainText(productName);
  const dashboardText = (await page.locator(".dashboard-page").textContent()) ?? "";
  expect(
    dashboardText.match(RAW_ID)?.[0] ?? null,
    "the dashboard should not print raw ids in any of its three tables",
  ).toBeNull();

  // The advanced-search page is gone: the builder is a modal over the bug
  // list now, so there is no second results table to check. The dashboard
  // assertion above already covers a table that had the same defect.
});
