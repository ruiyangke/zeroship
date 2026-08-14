import { expect, test } from "@playwright/test";

import { chooseOption } from "./select";
import { signIn } from "./session";
import { productKey } from "./keys";

/**
 * Advanced search is a modal over the bug list, not a page of its own.
 *
 * It used to be a separate route with its own results table, so running a
 * search left you somewhere other than the list you started from, looking at
 * a second table that had to be kept in step with the first -- and did not
 * always keep up: it was one of the three tables that rendered raw ids
 * because it never got the lookup props.
 *
 * The builder now writes its results back into the list you were already
 * reading.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);

const RUN = `${process.pid}-${Date.now()}`;

test("the builder runs a search into the bug list and hands it back", async ({
  page,
  baseURL,
  context,
}) => {
  await signIn(context, { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const product = await rpc("products.create", {
    name: `Modal ${RUN}`,
    key: productKey("MOD"),
    description: "modal",
  });
  const component = await rpc("components.create", {
    productId: product.id,
    name: "Core",
    description: "core",
  });
  const version = await rpc("versions.create", { productId: product.id, name: "1.0" });
  const summary = `Findable by builder ${RUN}`;
  await rpc("bugs.create", {
    productId: product.id,
    componentId: component.id,
    versionId: version.id,
    summary,
    description: "d",
  });

  await page.goto("/bugs");
  await expect(page.locator("table tbody tr").first()).toBeVisible();

  // There is no Search destination in the rail any more.
  await expect(
    page.getByRole("navigation").getByRole("link", { name: "Search" }),
    "advanced search is not a page",
  ).toHaveCount(0);

  await page.getByRole("button", { name: "Advanced..." }).click();
  const dialog = page.getByRole("dialog");
  await expect(dialog).toBeVisible();

  // Saved searches belongs to the builder, which binds it to the CURRENT
  // query. A second copy could only ever save an empty one.
  await expect(
    // The HEADING, not any text containing those words. getByText matches
    // case-insensitive substrings, so this also matched the empty state -- "No
    // saved searches yet." -- and counted two. It only ever passed because the
    // dev database always had a saved search left over from an earlier run;
    // the moment that database was rebuilt, the loose locator showed up as an
    // app defect it was not.
    dialog.getByRole("heading", { name: "Saved searches", exact: true }),
    "the saved-search section appears once",
  ).toHaveCount(1);

  await chooseOption(page, dialog, "Field", "summary");
  await chooseOption(page, dialog, "Operator", "contains");
  await dialog.getByLabel("Value").fill(RUN);
  await dialog.getByRole("button", { name: "Run search" }).click();

  // Closed, and the results are in the list rather than on another page.
  await expect(dialog, "running a search closes the builder").toBeHidden();
  await expect(page.getByText(/result.* from advanced search/i)).toBeVisible();
  await expect(page.getByText(summary)).toBeVisible();

  // And it is a mode you can leave, not a navigation you have to undo.
  await page.getByRole("button", { name: "Back to filters" }).click();
  await expect(page.getByText(/from advanced search/i)).toHaveCount(0);
  await expect(page.locator("table tbody tr").first()).toBeVisible();
});
