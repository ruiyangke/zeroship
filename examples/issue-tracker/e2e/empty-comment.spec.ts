import { expect, test } from "@playwright/test";

import { signIn } from "./session";
import { productKey } from "./keys";

/**
 * An emptied rich-text editor must not offer a live Comment button.
 *
 * The submit handler guarded on `hasText(body)`, but the button's `disabled`
 * guarded on `!body.trim()`. Those disagree exactly once: a tiptap document
 * you have typed into and then cleared serialises to "<p></p>", which is
 * non-empty to `trim()` and empty to `hasText`. So the button enabled itself
 * and the handler refused the post -- a control that looks live, accepts the
 * click, and does nothing.
 *
 * The empty case ALONE cannot catch this: a freshly mounted editor holds ""
 * and both guards agree, which is why the state has to be reached by typing
 * and deleting rather than by loading the page. The typed control below is
 * the one-variable partner -- same editor, same button, text present -- and
 * separates "the button is always disabled" from "the button tracks content".
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);
const RUN = `${process.pid}-${Date.now()}`;

test("clearing the comment editor disables the Comment button", async ({ page, baseURL }) => {
  await signIn(page.context(), { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const product = await rpc("products.create", {
    name: `Empty ${RUN}`,
    key: productKey("EMP"),
    description: "empty comment",
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
    summary: `Empty comment guard ${RUN}`,
    description: "<p>seed</p>",
  });

  await page.goto(`/bugs/${bug.id}`);

  const editor = page.getByLabel("Add a comment");
  const comment = page.getByRole("button", { name: "Comment", exact: true });
  await expect(editor).toBeVisible();

  // Freshly mounted: both guards agree, so this proves nothing on its own.
  await expect(comment, "an untouched editor offers no post").toBeDisabled();

  // The one-variable control: with text, the button must be live. Without
  // this, a button hardcoded to disabled would pass the assertion below.
  await editor.click();
  await page.keyboard.type("something");
  await expect(comment, "text present enables the post").toBeEnabled();

  // The regression: clear it. The document is now "<p></p>", not "".
  await page.keyboard.press("ControlOrMeta+A");
  await page.keyboard.press("Backspace");
  await expect(editor).toHaveText("");
  await expect(comment, "an emptied editor must not offer a live post").toBeDisabled();
});
