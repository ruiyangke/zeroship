import { expect, test, type Page } from "@playwright/test";

import { productKey } from "./keys";
import { chooseOption } from "./select";
import { signIn } from "./session";

/**
 * Every field in the app lights up the same way, including the two the design
 * system has no component for.
 *
 * The comment composer is a tiptap contenteditable and the description is a
 * bare `<textarea>`, so neither could be a DS `<Input>`. Both grew their own
 * chrome instead, and the copies drifted: 8px radius against 9.6px, and a
 * focus treatment that changed a border colour against one that did nothing
 * whatsoever. Measured on 2026-08-14, `.rich-text-editor` was BYTE-IDENTICAL
 * at rest and focused -- the most-used control on the issue page told a keyboard
 * user nothing about where they were.
 *
 * The fix is one `.app-field-shell` class built from the DS's published focus
 * tokens (`--zeroship-input-border-focus`, `--zeroship-input-focus-ring-color`,
 * `--zeroship-focus-ring-width`) -- the same ones Select, Combobox, NumberField and
 * Slider draw from.
 *
 * So the assertion is not "it changes on focus", which a wrong-coloured
 * one-pixel flicker would satisfy. It is that the shadow EQUALS the one a real
 * DS Input paints in the same state. A drift of any kind fails.
 *
 * WHAT THIS DOES NOT CATCH: it compares against a DS Input rendered in this
 * app, so a change to the DS itself moves both sides together and passes. That
 * is deliberate -- the claim is that the app's fields agree with the DS, not
 * that the DS ring is any particular colour, which is the DS's own test to
 * own.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);
const RUN = `${process.pid}-${Date.now()}`;

type Shadow = { rest: string; focus: string; radius: string };

/** Rest and focus box-shadow for one selector, focused by clicking `target`. */
async function shadows(page: Page, shell: string, target: string): Promise<Shadow> {
  const read = () =>
    page.evaluate((s) => {
      const el = document.querySelector(s);
      if (!el) throw new Error(`nothing matched ${s}`);
      const cs = getComputedStyle(el);
      // The FULL string. Truncating a box-shadow is what produced an earlier
      // regression here: the focus halo is the last component of the list, so
      // a slice drops exactly the part that differs and two clipped strings
      // compare equal. See input-chrome.spec.ts.
      return { shadow: cs.boxShadow, radius: cs.borderTopLeftRadius };
    }, shell);

  const before = await read();
  await page.click(target);
  // ASSERT THE FOCUS LANDED before measuring what focus looks like.
  //
  // Without this the spec compared shadows whether or not the click took, and
  // a miss produced a wall of oklab-versus-oklch colour text that reads as a
  // ring regression. It failed exactly that way once in a full suite run while
  // passing 6/6 in isolation: under load the form can still be settling, and a
  // re-render after the click drops focus. The distinction matters -- "the
  // ring is wrong" and "nothing was focused" need different fixes, and only
  // this line tells them apart.
  await expect(
    page.locator(target),
    "the click actually focused the field, so the measurement below is of a focused control",
  ).toBeFocused();
  await page.waitForTimeout(350);
  const after = await read();
  return { rest: before.shadow, focus: after.shadow, radius: before.radius };
}

test("the app's own fields wear the design system's focus ring", async ({ page, baseURL }) => {
  await signIn(page.context(), { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const product = await rpc("products.create", {
    name: `Ring ${RUN}`,
    key: productKey("RNG"),
    description: "ring",
  });
  const component = await rpc("components.create", {
    productId: product.id,
    name: "Parser",
    description: "parser",
  });
  const version = await rpc("versions.create", { productId: product.id, name: "1.0" });
  const issue = await rpc("issues.create", {
    productId: product.id,
    componentId: component.id,
    versionId: version.id,
    summary: `Ring issue ${RUN}`,
    description: "d",
  });

  // The reference: a real DS Input, in this app, in this theme.
  await page.goto("/issues");
  await page.locator("input[data-slot~=\"input-control\"]").first().waitFor();
  const ds = await shadows(page, "[data-slot~=\"input\"]", "input[data-slot~=\"input-control\"]");
  expect(ds.rest, "the reference Input actually changes on focus").not.toBe(ds.focus);
  expect(ds.focus, "and specifically paints the 3px halo").toMatch(/0px 0px 0px 3px/);

  // 1. The comment composer -- a contenteditable, so focus lands on a CHILD of
  //    the shell. A rule keyed on the shell itself never fires, which is the
  //    exact shape of the bug this spec exists for.
  await page.goto(`/issues/${issue.id}`);
  await page.locator(".rich-text-editor").first().waitFor();
  const editor = await shadows(page, ".rich-text-editor", ".rich-text-input");
  expect(editor.rest, "the composer rests like a DS field").toBe(ds.rest);
  expect(editor.focus, "and focuses like one -- it used to be identical to its rest").toBe(ds.focus);
  expect(editor.radius, "same corner as a DS field, not a hand-picked 0.6rem").toBe(ds.radius);

  // 2. The description textarea, which only exists once a product is chosen.
  await page.goto("/issues/new");
  await chooseOption(page, page, "Product", `Ring ${RUN}`);
  await chooseOption(page, page, "Component", "Parser");
  const area = await shadows(page, "textarea.app-textarea", "textarea.app-textarea");
  expect(area.rest, "the description rests like a DS field").toBe(ds.rest);
  expect(area.focus, "and focuses like one").toBe(ds.focus);
  expect(area.radius, "same corner as a DS field").toBe(ds.radius);
});
