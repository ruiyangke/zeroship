import { expect, test } from "@playwright/test";

import { productKey } from "./keys";
import { openRailGroup } from "./more";
import { signIn } from "./session";

/**
 * You can name an issue by the identifier the app shows you.
 *
 * `issues.get` already says why, in a comment: "Refusing the human form here
 * would mean the identifier the app puts on screen is not one it accepts
 * back." Every screen calls this issue PARSER-12. Nothing calls it
 * issu_0346W0Ole6amXDN9RKvzW6 -- that string appears in an href and nowhere a
 * person could read it.
 *
 * The principle was applied to issues.get alone. Adding a dependency and marking
 * a duplicate both take an issue reference, and both demanded the UUID, with
 * placeholders that said so ("issu_..."). So the two places you must name
 * another issue were the two places you could not name it the way the app does.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);
const RUN = `${process.pid}-${Date.now()}`;

test("an issue is referenced by its key, and shown by its key", async ({ page, baseURL }) => {
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
    rpc("issues.create", {
      productId: product.id,
      componentId: component.id,
      versionId: version.id,
      summary,
      description: "d",
    });

  const issue = await make(`Refers to others ${RUN}`);
  const blocker = await make(`Blocker ${RUN}`);
  const original = await make(`Original ${RUN}`);

  // The keys, exactly as the page prints them above the summary.
  const blockerKey = `${key}-${blocker.number}`;
  const originalKey = `${key}-${original.number}`;

  // The "not by a raw id" assertion at the end of this test is only a
  // constraint while raw ids really look like RAW_ISSUE_ID, and that shape is
  // NOT stable: it is derived from the collection name at runtime
  // (`derive_prefix_from_collection_name` in
  // crates/plugin-db/src/crud/system_fields_pass.rs -- strip a trailing "s",
  // first four alphanumerics, lowercase), so renaming `bugs` to `issues` moved
  // it from `bug_` to `issu_`. A regex left on the old prefix cannot match
  // anything the app mints, and the assertion passes forever while checking
  // nothing. Pinning it against a live id is what makes that fail loudly.
  const RAW_ISSUE_ID = /\bissu_[A-Za-z0-9]{12,}/;
  expect(
    issue.id,
    "minted ids no longer look like RAW_ISSUE_ID, so the absence check below is vacuous",
  ).toMatch(RAW_ISSUE_ID);

  await page.goto(`/issues/${issue.id}`);

  // 1. A dependency, added by key through the real control.
  await openRailGroup(page, "Dependencies");
  const deps = page.locator("section.relations-panel").first();
  await deps.getByLabel("Issue this depends on").fill(blockerKey);
  await deps.getByRole("button", { name: "Add dependency" }).click();
  await expect(
    deps,
    "the dependency was accepted by key rather than refused as a bad id",
  ).toContainText(`Blocker ${RUN}`);

  // 2. A duplicate, likewise.
  await page.getByRole("button", { name: /Mark as duplicate/ }).click();
  await page.getByLabel(/Duplicate of/).fill(originalKey);
  await page.getByRole("button", { name: "Confirm duplicate" }).click();

  // 3. And the resulting statement names the issue the way the app does.
  await openRailGroup(page, "Duplicates");
  const dupes = page.locator("section.relations-panel").filter({ hasText: "duplicate of" });
  await expect(dupes, "the duplicate is named by key").toContainText(originalKey);
  await expect(
    dupes,
    "and not by an id no screen ever showed",
  ).not.toContainText(RAW_ISSUE_ID);
});
