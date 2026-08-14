import { expect, test } from "@playwright/test";

import { productKey } from "./keys";
import { signIn } from "./session";

/**
 * A filtered list is a link you can send.
 *
 * The filters were component state, so narrowing the list changed what you
 * saw and nothing else: the URL stayed /bugs, a reload dropped it, Back did
 * not undo it, and pasting the address to a colleague sent them the unfiltered
 * list. That is the half of routing the hash never made worth doing.
 *
 * Asserts the round trip -- set it, read the URL, open that URL COLD in a
 * fresh page, and get the same rows. Checking only that the URL changed would
 * pass on a query the app writes and never reads.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);
const RUN = `${process.pid}-${Date.now()}`;

test("a filtered list survives a reload and travels in a link", async ({
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
    name: `Share ${RUN}`,
    key: productKey("SHR"),
    description: "share",
  });
  const component = await rpc("components.create", {
    productId: product.id,
    name: "Core",
    description: "core",
  });
  const version = await rpc("versions.create", { productId: product.id, name: "1.0" });
  const mk = async (summary: string, severity: string) => {
    const bug = await rpc("bugs.create", {
      productId: product.id,
      componentId: component.id,
      versionId: version.id,
      summary,
      description: "d",
    });
    await rpc("bugs.setSeverity", { id: bug.id, severity });
    return bug;
  };
  await mk(`Critical one ${RUN}`, "critical");
  await mk(`Normal one ${RUN}`, "normal");

  await page.goto("/bugs");

  // Narrow to this product, then to critical.
  const product_ = page.getByRole("combobox", { name: /product/i }).first();
  await product_.click();
  await page.getByRole("option", { name: `Share ${RUN}` }).click();
  const severity = page.getByRole("combobox", { name: /severity/i }).first();
  await severity.click();
  await page.getByRole("option", { name: "critical", exact: true }).click();

  await expect(page.getByText(`Critical one ${RUN}`)).toBeVisible();
  await expect(page.getByText(`Normal one ${RUN}`)).toHaveCount(0);

  // The URL says what you are looking at.
  const shared = page.url();
  expect(shared, "the severity is in the query").toContain("severity=critical");
  expect(shared, "and so is the product").toContain("product=");

  // COLD: a fresh page, as a colleague opening the link would.
  const fresh = await context.newPage();
  await fresh.goto(shared);
  await expect(
    fresh.getByText(`Critical one ${RUN}`),
    "the link reproduces the same list for someone else",
  ).toBeVisible();
  await expect(
    fresh.getByText(`Normal one ${RUN}`),
    "still narrowed, so the query was read and not merely written",
  ).toHaveCount(0);
  await fresh.close();

  // Clearing removes EVERY filter, not just the last one written.
  await page.getByRole("button", { name: /clear/i }).first().click();
  await expect(page.getByText(`Normal one ${RUN}`)).toBeVisible();
  const cleared = new URL(page.url());
  expect(
    [...cleared.searchParams.keys()],
    "clear leaves no filter behind in the URL",
  ).toEqual([]);
});
