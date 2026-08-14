import { expect, test } from "@playwright/test";

import { productKey } from "./keys";
import { signIn } from "./session";

/**
 * Abandoning a comment edit discards it.
 *
 * The editor had a Save button and nothing else, so the only exit was the
 * button labelled "Edit" -- which reads as "start editing", not "throw this
 * away" -- and taking it KEPT the draft. Reopening showed the abandoned text
 * sitting in the box, indistinguishable from the comment's real body, which is
 * how someone ends up saving an edit they thought they had cancelled.
 *
 * The saved case is asserted alongside it. "Cancel discards" would pass just
 * as well against an editor that discarded everything, including Save.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);
const RUN = `${process.pid}-${Date.now()}`;

test("cancelling a comment edit throws the draft away, saving keeps it", async ({
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
    name: `Edit ${RUN}`,
    key: productKey("EDT"),
    description: "edit",
  });
  const component = await rpc("components.create", {
    productId: product.id,
    name: "Core",
    description: "core",
  });
  const version = await rpc("versions.create", { productId: product.id, name: "1.0" });
  const bug = await rpc("bugs.create", {
    productId: product.id,
    componentId: component.id,
    versionId: version.id,
    summary: `Edit cancel ${RUN}`,
    description: "seed",
  });
  const original = `original-${RUN}`;
  await rpc("comments.add", { bugId: bug.id, body: original });

  await page.goto(`/bugs/${bug.id}`);
  // Anchored to the comment element, NOT to its text. Filtering by the body
  // and then replacing that body in the editor makes the locator stop matching
  // the very element being driven -- the Cancel button then "does not exist"
  // for a reason that has nothing to do with Cancel.
  const comment = page.locator("li#comment-1");
  await expect(comment).toHaveCount(1);
  await expect(comment).toContainText(original);

  // Edit, type something else, cancel.
  await comment.getByRole("button", { name: /^Edit comment/ }).click();
  const editor = comment.getByLabel("Edit comment", { exact: true });
  await editor.click();
  await page.keyboard.press("ControlOrMeta+A");
  await page.keyboard.type("abandoned text");
  await comment.getByRole("button", { name: /^Cancel editing/ }).click();

  await expect(comment, "the comment still reads as it did").toContainText(original);
  await expect(comment, "and the abandoned text is gone").not.toContainText("abandoned text");

  // Reopening must not resurrect the discarded draft -- this is the half that
  // was actually broken; the display above was already correct.
  await comment.getByRole("button", { name: /^Edit comment/ }).click();
  await expect(
    comment.getByLabel("Edit comment", { exact: true }),
    "the editor reopens on the real body, not the discarded draft",
  ).toContainText(original);
  await expect(comment.getByLabel("Edit comment", { exact: true })).not.toContainText("abandoned text");

  // The control: saving still works, so "discard" is not just "never persist".
  await comment.getByLabel("Edit comment", { exact: true }).click();
  await page.keyboard.press("ControlOrMeta+A");
  await page.keyboard.type("kept text");
  await comment.getByRole("button", { name: "Save", exact: true }).click();
  await expect(
    page.locator("li.comment", { hasText: "kept text" }),
    "a saved edit persists",
  ).toHaveCount(1);
});
