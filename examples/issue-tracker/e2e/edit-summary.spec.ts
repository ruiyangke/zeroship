import { expect, test } from "@playwright/test";

import { productKey } from "./keys";
import { signIn } from "./session";

/**
 * The issue's title is edited where the title is.
 *
 * This lived in the metadata rail as "Edit summary" -- a control several
 * hundred pixels from the words it changes, in the column reserved for facts
 * ABOUT the issue. A title is not metadata about itself.
 *
 * The affordance is quiet until the heading is hovered or focused, so this
 * spec drives it the way a keyboard user reaches it rather than by forcing a
 * click on a hidden element: if focus does not reveal it, the control is
 * unreachable without a mouse and the test should say so.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);
const RUN = `${process.pid}-${Date.now()}`;

test("the summary is edited from the page head, and can be abandoned", async ({
  page,
  baseURL,
}) => {
  await signIn(page.context(), { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const product = await rpc("products.create", {
    name: `Title ${RUN}`,
    key: productKey("TTL"),
    description: "title",
  });
  const component = await rpc("components.create", {
    productId: product.id,
    name: "Core",
    description: "core",
  });
  const version = await rpc("versions.create", { productId: product.id, name: "1.0" });
  const original = `Original summary ${RUN}`;
  const issue = await rpc("issues.create", {
    productId: product.id,
    componentId: component.id,
    versionId: version.id,
    summary: original,
    description: "seed",
  });

  await page.goto(`/issues/${issue.id}`);
  await expect(page.locator("h1")).toContainText(original);

  // Reachable by keyboard: focusing the control is what reveals it.
  const edit = page.getByRole("button", { name: "Edit summary" });
  await edit.focus();
  await expect(edit, "the affordance appears on focus, not only on hover").toBeVisible();

  // Abandon first, so a later success cannot be mistaken for it never having
  // been exercised.
  await edit.click();
  const field = page.getByLabel("Summary");
  await field.fill(`Abandoned ${RUN}`);
  await page.getByRole("button", { name: "Cancel", exact: true }).click();
  await expect(page.locator("h1"), "cancelling leaves the title alone").toContainText(original);

  // And it really saves.
  await edit.focus();
  await edit.click();
  const updated = `Updated summary ${RUN}`;
  await page.getByLabel("Summary").fill(updated);
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.locator("h1"), "saving rewrites the heading").toContainText(updated);

  // Persisted, not just re-rendered.
  const stored = await rpc("issues.get", { id: issue.id });
  expect(stored.issue.summary, "and the new summary is stored").toBe(updated);
});
