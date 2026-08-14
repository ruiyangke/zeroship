import { expect, test } from "@playwright/test";

import { signIn } from "./session";
import { productKey } from "./keys";

/**
 * The toolbar's link control, including the path where the URL is refused.
 *
 * `setLink` applies the same protocol allowlist that guards rendering, so a
 * `javascript:` URL never becomes a mark. The failure mode being guarded here
 * is not the security one -- that is e2e/comment-xss.spec.ts -- it is the
 * INTERFACE one: a control that takes your input, closes, and produces nothing
 * reads as a broken button rather than a rejected URL.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);
const RUN = `${process.pid}-${Date.now()}`;

test("the link control applies safe URLs and states refusals", async ({ page, baseURL }) => {
  await signIn(page.context(), { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const product = await rpc("products.create", {
    name: `Link ${RUN}`,
    key: productKey("LNK"),
    description: "link",
  });
  const component = await rpc("components.create", {
    productId: product.id,
    name: "Core",
    description: "core",
  });
  const version = await rpc("versions.create", { productId: product.id, name: "1.0" });
  const issue = await rpc("issues.create", {
    productId: product.id,
    componentId: component.id,
    versionId: version.id,
    summary: `Link control ${RUN}`,
    description: "<p>seed</p>",
  });

  await page.goto(`/issues/${issue.id}`);

  const editor = page.getByLabel("Add a comment");
  const composer = page.locator(".new-comment");
  await expect(editor).toBeVisible();

  // The composer's toolbar stays out of the way until there is something to
  // format. Both halves are asserted: absent while blurred and empty, present
  // once focused. The first alone would pass against a toolbar that never
  // renders at all.
  await expect(
    composer.getByRole("button", { name: "Bold" }),
    "an untouched composer shows no formatting controls",
  ).toHaveCount(0);

  // Type text and select it -- a link needs something to attach to.
  await editor.click();
  await expect(
    composer.getByRole("button", { name: "Bold" }),
    "focusing the composer brings the toolbar",
  ).toBeVisible();
  await page.keyboard.type("see the docs");
  await page.keyboard.press("ControlOrMeta+A");

  await composer.getByRole("button", { name: "Link", exact: true }).click();
  const url = composer.getByLabel("Link URL");
  await expect(url).toBeVisible();

  // The refusal path first, so a later success cannot be mistaken for it
  // never having been exercised.
  await url.fill("javascript:alert(1)");
  await composer.getByRole("button", { name: "Apply" }).click();
  await expect(
    composer.getByRole("alert"),
    "a refused URL is stated, not silently dropped",
  ).toContainText(/refused/i);
  await expect(
    editor.locator("a"),
    "and no link was created by the refused URL",
  ).toHaveCount(0);

  // The one-variable partner: same control, same selection, safe scheme.
  // Without it, a control that refuses EVERYTHING would pass the above.
  await url.fill("https://example.com/docs");
  await composer.getByRole("button", { name: "Apply" }).click();
  await expect(url, "the row closes once the link applies").toHaveCount(0);
  const link = editor.locator('a[href="https://example.com/docs"]');
  await expect(link, "a safe URL becomes a link").toHaveCount(1);
  await expect(link).toHaveText("see the docs");

  // Posting round-trips it through storage and back through the renderer.
  await page.getByRole("button", { name: "Comment", exact: true }).click();
  await expect(
    page.locator('li.comment a[href="https://example.com/docs"]'),
    "the link survives the store and re-render",
  ).toHaveCount(1);
});
