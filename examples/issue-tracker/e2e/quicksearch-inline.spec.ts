import { expect, test } from "@playwright/test";

import { signIn } from "./session";

/**
 * The QuickSearch shorthand works in the list's own search box.
 *
 * It used to have a box on a page of its own. Folding advanced search into a
 * modal removed that page and left `QuickSearchBox` defined but rendered
 * nowhere -- `search.quick` became unreachable from the UI, the same dead-
 * feature shape the flags surface had. Nothing failed: an orphaned component
 * still compiles, and no test asked for it.
 *
 * The tokens are parsed into the SAME filters the dropdowns drive, so a typed
 * query and a clicked one produce one request and one set of chips.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);

const RUN = `${process.pid}-${Date.now()}`;

test("typing P1 in the search box filters by priority", async ({ page, baseURL, context }) => {
  await signIn(context, { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const product = await rpc("products.create", {
    name: `Quick ${RUN}`,
    key: `QCK${String(Date.now()).slice(-5)}`,
    description: "quick",
  });
  const component = await rpc("components.create", {
    productId: product.id,
    name: "Core",
    description: "core",
  });
  const version = await rpc("versions.create", { productId: product.id, name: "1.0" });
  // One of each priority, so filtering to P1 is a real narrowing rather than
  // a query that happens to match everything.
  for (const priority of ["P1", "P3"] as const) {
    const bug = await rpc("bugs.create", {
      productId: product.id,
      componentId: component.id,
      versionId: version.id,
      summary: `Quick ${priority} ${RUN}`,
      description: "d",
    });
    await rpc("bugs.setPriority", { id: bug.id, priority });
  }

  await page.goto("/#/bugs");
  const search = page.getByPlaceholder("Search, or type");

  // The run marker alone: both bugs.
  await search.fill(RUN);
  await page.getByRole("button", { name: "Apply" }).click();
  await expect(page.locator("table tbody tr")).toHaveCount(2);

  // With the shorthand: one, and the token is visible as a chip so the
  // narrowing is not silent.
  await search.fill(`P1 ${RUN}`);
  await page.getByRole("button", { name: "Apply" }).click();
  await expect(page.locator("table tbody tr")).toHaveCount(1);
  await expect(page.getByText(`Quick P1 ${RUN}`)).toBeVisible();
  await expect(page.getByText(/Priority: P1 \(typed\)/)).toBeVisible();

  // A half-typed query is the normal state of a search box. "@" with no
  // handle throws in the parser, and a throw here would blank the list
  // mid-keystroke rather than simply matching nothing.
  await search.fill(`${RUN} @`);
  await page.getByRole("button", { name: "Apply" }).click();
  // The page survives -- which is not the same as "a table is shown". An
  // unparseable query matches nothing, and DataTable renders its empty state
  // rather than a table, so asserting on <table> fails on correct behaviour.
  // Level 1 specifically: the rail also has a "Bugs" link and the empty
  // state carries its own heading, so an unlevelled match is ambiguous.
  await expect(page.getByRole("heading", { level: 1, name: "Bugs" })).toBeVisible();
  await expect(page.getByText(/something went wrong/i)).toHaveCount(0);
});
