import { expect, test } from "@playwright/test";

import { productKey } from "./keys";
import { openRailGroup } from "./more";
import { signIn } from "./session";

/**
 * You can name a bug by the identifier the app shows you.
 *
 * `bugs.get` already says why, in a comment: "Refusing the human form here
 * would mean the identifier the app puts on screen is not one it accepts
 * back." Every screen calls this bug PARSER-12. Nothing calls it
 * bug_0346W0Ole6amXDN9RKvzW6 -- that string appears in an href and nowhere a
 * person could read it.
 *
 * The principle was applied to bugs.get alone. Adding a dependency and marking
 * a duplicate both take a bug reference, and both demanded the UUID, with
 * placeholders that said so ("bug_..."). So the two places you must name
 * another bug were the two places you could not name it the way the app does.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);
const RUN = `${process.pid}-${Date.now()}`;

test("a bug is referenced by its key, and shown by its key", async ({ page, baseURL }) => {
  await signIn(page.context(), { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const key = productKey("KEY");
  const product = await rpc("products.create", {
    name: `Keyed ${RUN}`,
    key,
    description: "keyed",
  });
  const component = await rpc("components.create", {
    productId: product.id,
    name: "Core",
    description: "core",
  });
  const version = await rpc("versions.create", { productId: product.id, name: "1.0" });
  const make = (summary: string) =>
    rpc("bugs.create", {
      productId: product.id,
      componentId: component.id,
      versionId: version.id,
      summary,
      description: "d",
    });

  const bug = await make(`Refers to others ${RUN}`);
  const blocker = await make(`Blocker ${RUN}`);
  const original = await make(`Original ${RUN}`);

  // The keys, exactly as the page prints them above the summary.
  const blockerKey = `${key}-${blocker.number}`;
  const originalKey = `${key}-${original.number}`;

  await page.goto(`/bugs/${bug.id}`);

  // 1. A dependency, added by key through the real control.
  await openRailGroup(page, "Dependencies");
  const deps = page.locator("section.relations-panel").first();
  await deps.getByLabel("Bug this depends on").fill(blockerKey);
  await deps.getByRole("button", { name: "Add dependency" }).click();
  await expect(
    deps,
    "the dependency was accepted by key rather than refused as a bad id",
  ).toContainText(`Blocker ${RUN}`);

  // 2. A duplicate, likewise.
  await page.getByRole("button", { name: /Mark as duplicate/ }).click();
  await page.getByLabel(/Duplicate of/).fill(originalKey);
  await page.getByRole("button", { name: "Confirm duplicate" }).click();

  // 3. And the resulting statement names the bug the way the app does.
  await openRailGroup(page, "Duplicates");
  const dupes = page.locator("section.relations-panel").filter({ hasText: "duplicate of" });
  await expect(dupes, "the duplicate is named by key").toContainText(originalKey);
  await expect(
    dupes,
    "and not by an id no screen ever showed",
  ).not.toContainText(/\bbug_[A-Za-z0-9]{12,}/);
});
