import { expect, test } from "@playwright/test";

import { signIn } from "./session";

/**
 * The id column has one job: let a reader tell two rows apart and quote one.
 *
 * This is a browser spec rather than a unit test on the formatter because the
 * defect it guards was invisible in every other signal. The formatter returned
 * a string, the column rendered, no request failed and no console error fired
 * -- the app looked healthy in the page walk. It is only when several bugs are
 * on screen together that the column turns out to say the same thing about all
 * of them, and "on screen together" is the condition, so the browser is where
 * the assertion belongs.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);

const RUN = `${process.pid}-${Date.now()}`;

test("every bug in the list shows a distinct id", async ({ page, baseURL, context }) => {
  await signIn(context, { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const product = await rpc("products.create", { name: `Ident ${RUN}`, description: "identity" });
  const component = await rpc("components.create", {
    productId: product.id,
    name: "Core",
    description: "core",
  });
  const version = await rpc("versions.create", { productId: product.id, name: "1.0" });

  // Filed back to back ON PURPOSE. A typed_id is a time-ordered UUIDv7 in
  // base62, so ids minted in the same window share a long leading run and
  // differ only at the tail. Spacing these out would hide the very thing under
  // test; a burst is also what a test run, a script or an import produces.
  const created: string[] = [];
  for (let i = 0; i < 8; i += 1) {
    const bug = await rpc("bugs.create", {
      productId: product.id,
      componentId: component.id,
      versionId: version.id,
      summary: `Identity ${i} ${RUN}`,
      description: "identity probe",
    });
    created.push(bug.id);
  }
  expect(new Set(created).size, "the server must mint distinct ids to begin with").toBe(
    created.length,
  );

  // Narrow the list to this run's product so the assertion is about these
  // eight rows and cannot be satisfied by unrelated bugs already in the dev
  // database.
  await page.goto("/#/bugs");
  await page.getByPlaceholder("Search summary").fill(RUN);
  await page.getByRole("button", { name: "Apply" }).click();

  const idCells = page.locator("table tbody tr td[data-col='id']");
  await expect(idCells).toHaveCount(created.length);

  const shown = (await idCells.allTextContents()).map((t) => t.trim());
  expect(new Set(shown).size, `the id column repeats itself: ${JSON.stringify(shown)}`).toBe(
    shown.length,
  );

  // Uniqueness alone would also be satisfied by a column rendering row numbers
  // or the summary. Each cell must be the bug's OWN id: shortened, but a
  // genuine tail of the id its link points at.
  for (const cell of await idCells.all()) {
    const text = (await cell.textContent())?.trim() ?? "";
    const link = cell.locator("a");
    const href = (await link.getAttribute("href")) ?? "";
    const full = href.split("/").pop() ?? "";
    expect(created, "each row should be one of the bugs filed above").toContain(full);
    expect(
      full.endsWith(text.replace(/^.*\.\.\./, "")),
      `"${text}" is not a tail of ${full}`,
    ).toBe(true);
    // The full id must stay reachable, since the visible form is lossy -- and
    // it is checked per row against that row's own link rather than against a
    // position in `created`. Rows come back ordered by update time, which for
    // bugs filed in one burst is not the order they were created in; asserting
    // on `created[last]` failed here for that reason alone, while the column
    // was correct.
    await expect(link, "the row must carry its own full id").toHaveAttribute("title", full);
  }
});
