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

  // An explicit key, because a derived one would be the run marker's digits
  // and collide with another run's product.
  const productKey = `ID${String(process.pid).slice(-5)}`;
  const product = await rpc("products.create", {
    name: `Ident ${RUN}`,
    key: productKey,
    description: "identity",
  });
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
  // or a UUID tail. A per-product sequence is the claim, so the sequence is
  // what gets checked: every label is KEY-N for this product, and the eight
  // numbers are exactly 1..8 with no gap and no repeat.
  const numbers = shown
    .map((label) => {
      expect(label, `"${label}" should be ${productKey}-N`).toMatch(
        new RegExp(`^${productKey}-\\d+$`),
      );
      return Number(label.slice(productKey.length + 1));
    })
    .sort((left, right) => left - right);
  expect(numbers, "the sequence runs 1..8 with no gaps").toEqual([1, 2, 3, 4, 5, 6, 7, 8]);

  // The key is what a person reads; the UUID is still the identity. Each row
  // must link to one of the bugs filed above, checked per row against that
  // row's own link rather than a position in `created` -- rows come back
  // ordered by update time, which for a burst is not creation order.
  for (const cell of await idCells.all()) {
    const href = (await cell.locator("a").getAttribute("href")) ?? "";
    expect(created, "each row links to one of the bugs filed above").toContain(
      href.split("/").pop(),
    );
  }
});
