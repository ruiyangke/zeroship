import { expect, test } from "@playwright/test";

import { signIn } from "./session";
import { chooseOption } from "./select";

/**
 * The component report must let a reader tell its rows apart.
 *
 * Component names are unique only within a product, and every product calls
 * its first component something like "Core". Across products the report
 * therefore renders many rows with identical labels and different counts,
 * which reads as a broken query rather than as real data.
 *
 * A browser spec, for the same reason as issue-list-identity: the RPC payload is
 * correct -- each row carries its own component with its own productId -- so
 * nothing server-side is wrong to assert on. The defect exists only in what
 * reaches the screen.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);

const RUN = `${process.pid}-${Date.now()}`;

test("the component report distinguishes same-named components across products", async ({
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

  // Two products, each with a component of the SAME name. This is the ordinary
  // case, not a contrived one -- "Core" is what both of them would be called.
  const productNames = [`Alpha ${RUN}`, `Beta ${RUN}`];
  const created: { product: string; componentId: string }[] = [];
  for (const name of productNames) {
    const product = await rpc("products.create", { name, description: "labels" });
    const component = await rpc("components.create", {
      productId: product.id,
      name: "Core",
      description: "core",
    });
    const version = await rpc("versions.create", { productId: product.id, name: "1.0" });
    await rpc("issues.create", {
      productId: product.id,
      componentId: component.id,
      versionId: version.id,
      summary: `Report probe ${name}`,
      description: "probe",
    });
    created.push({ product: name, componentId: component.id });
  }

  await page.goto("/reports");
  const section = page.locator("section.report-section").filter({ hasText: "By component" });
  await expect(section).toBeVisible();

  const summary = page.locator("section.report-section").filter({ hasText: "By status" });
  const countIndicators = summary.locator('[data-slot~="progress-indicator"]');
  expect(await countIndicators.count(), "the summary rendered count bars to inspect").toBeGreaterThan(0);
  const countColours = await countIndicators.evaluateAll((elements) =>
    elements.map((element) => getComputedStyle(element).backgroundColor),
  );
  const neutralCountColour = await summary.evaluate(() => {
    const probe = document.createElement("span");
    probe.style.backgroundColor = "var(--it-ink-muted)";
    document.body.appendChild(probe);
    const result = getComputedStyle(probe).backgroundColor;
    probe.remove();
    return result;
  });
  expect(
    [...new Set(countColours)],
    "relative counts use one neutral ink instead of claiming success at the maximum",
  ).toEqual([neutralCountColour]);

  const labels = (await section.locator("tbody tr td:first-child").allTextContents()).map((t) =>
    t.trim(),
  );

  // Over the WHOLE table, not just this run's rows. Filtering to rows
  // containing RUN would find them by the product name the fix adds, so on the
  // pre-fix code the filter matches nothing and the failure reads "0 rows
  // found" -- indistinguishable from a report that returned nothing at all.
  // Every row being distinguishable is the property anyway.
  const repeated = labels.filter((l, i) => labels.indexOf(l) !== i);
  expect(new Set(repeated).size, `these labels name more than one row: ${JSON.stringify([
    ...new Set(repeated),
  ])}`).toBe(0);

  // Distinctness alone would be satisfied by appending a row number. Each
  // label has to name the product the component actually belongs to.
  for (const { product } of created) {
    expect(labels, `no row is labelled for ${product}`).toContain(`${product} / Core`);
  }

  // The qualifier is conditional, so the unambiguous case needs its own check:
  // filtered to one product, the name stands alone rather than repeating the
  // product already chosen in the filter.
  await chooseOption(page, page, "Product", productNames[0]);
  await expect(section.locator("tbody tr")).toHaveCount(1);
  await expect(section.locator("tbody tr td:first-child")).toHaveText("Core");
});
