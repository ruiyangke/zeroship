import { expect, test } from "@playwright/test";

import { signIn } from "./session";

/**
 * Sorting happens on the table headers, and only on columns the SERVER can
 * order by.
 *
 * The page used to carry a separate "Sort: [column] [direction]" row, so the
 * control naming a column sat away from the column itself and could offer to
 * sort by something the visible table did not even show.
 *
 * The important half is the negative one. `DataTable` will happily sort any
 * column client-side, and the list is paginated at 25 -- a client sort would
 * reorder the current page and leave the other pages untouched, which looks
 * like sorting and is not. Only columns with a server sort key are clickable.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);

const RUN = `${process.pid}-${Date.now()}`;

test("headers sort the server query, and only where the server can", async ({
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

  const key = `SORT${String(Date.now()).slice(-5)}`;
  const product = await rpc("products.create", {
    name: `Sort ${RUN}`,
    key,
    description: "sorting",
  });
  const component = await rpc("components.create", {
    productId: product.id,
    name: "Core",
    description: "core",
  });
  const version = await rpc("versions.create", { productId: product.id, name: "1.0" });

  // Filed in ASCENDING priority order so the created order and the sorted
  // order disagree. Filing them already sorted would let a table that ignores
  // the click still pass.
  for (const priority of ["P1", "P3", "P2"] as const) {
    const bug = await rpc("bugs.create", {
      productId: product.id,
      componentId: component.id,
      versionId: version.id,
      summary: `Sortable ${priority} ${RUN}`,
      description: "d",
    });
    await rpc("bugs.setPriority", { id: bug.id, priority });
  }

  await page.goto("/#/bugs");
  await page.getByPlaceholder("Search summary").fill(RUN);
  await page.getByRole("button", { name: "Apply" }).click();

  const priorities = async () =>
    (await page.locator("table tbody tr td[data-column='priority']").allTextContents()).map((t) =>
      t.trim(),
    );
  await expect(page.locator("table tbody tr")).toHaveCount(3);

  await page.getByRole("button", { name: /^Priority/ }).click();
  const ascending = await priorities();
  expect(ascending, "clicking Priority orders by it").toEqual([...ascending].sort());

  await page.getByRole("button", { name: /^Priority/ }).click();
  const descending = await priorities();
  expect(descending, "clicking again reverses it").toEqual([...ascending].reverse());

  // Summary has no server sort key, so its header must be plain text rather
  // than a button offering a sort the server cannot honour.
  const summaryHeader = page.locator("table thead th", { hasText: "Summary" });
  await expect(summaryHeader).toBeVisible();
  await expect(
    summaryHeader.locator("button"),
    "Summary is not server-sortable, so its header must not be clickable",
  ).toHaveCount(0);
});
