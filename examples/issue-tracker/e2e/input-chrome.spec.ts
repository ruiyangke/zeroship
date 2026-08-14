import { expect, test } from "@playwright/test";

import { signIn } from "./session";

/**
 * The design system owns a field's edges. The app adds none.
 *
 * Reported twice as "dual borders on all the inputs", the second time worse
 * than the first, and both times the extra edge came from this app.
 *
 * Round one: bare element selectors.
 *
 *   input, select, textarea { border: 1px solid ... }
 *   input:focus, ...        { outline: 2px solid ...; border-color: ... }
 *
 * An element selector matches the DS control too. At rest its class won, so
 * one ring showed; on focus the outline had nothing to beat and landed on top
 * of the wrapper's ring.
 *
 * Round two was mine. Removing those rules, I measured whether the DS had a
 * focus ring of its own by printing the wrapper's box-shadow at rest and at
 * focus -- truncated to 120 characters. The halo is the LAST component of that
 * shadow list, so the truncation removed exactly the part that differed. Two
 * clipped strings compared equal, I concluded the DS drew nothing on focus,
 * and I added an outline back. That made three edges.
 *
 * What the DS actually draws on `.zs-input[data-focused]`:
 *
 *   inset 0 0 0 0.0625rem var(--zs-input-border-focus)   accent hairline
 *   0 0 0 0.1875rem var(--zs-input-focus-ring-color)     soft 16% halo
 *
 * and `.zs-input__control { outline: none }`, deliberately, so the wrapper
 * owns the ring and the control and slots share it.
 *
 * THE INVARIANT, and note it is the OPPOSITE of what this spec asserted after
 * round one. That version required `outline > 0` on the focused control -- it
 * encoded the regression as a requirement and passed the whole time it was
 * broken. A test can hold a bug in place.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);

test("the app adds no edge of its own to a design-system field", async ({ page, baseURL }) => {
  await signIn(page.context(), { runtimePort: RUNTIME_PORT, baseURL: baseURL! });
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto("/#/bugs");

  const control = page.locator("input.zs-input__control").first();
  await expect(control).toBeVisible();

  const read = () =>
    page.evaluate(() => {
      const el = document.querySelector("input.zs-input__control") as HTMLElement;
      const wrapper = el.closest(".zs-input") as HTMLElement;
      const cs = getComputedStyle(el);
      const ws = getComputedStyle(wrapper);
      return {
        focused: wrapper.hasAttribute("data-focused"),
        controlBorder: parseFloat(cs.borderTopWidth) || 0,
        controlOutline: parseFloat(cs.outlineWidth) || 0,
        // FULL string. Truncating this is what caused round two.
        wrapperShadow: ws.boxShadow,
      };
    });

  const rest = await read();
  expect(rest.controlBorder, "the control carries no border; the wrapper draws it").toBe(0);
  expect(rest.controlOutline, "and no outline at rest").toBe(0);

  // Focus it the way a person does. Tabbing was unreliable here -- an earlier
  // probe "focused" nothing and reported data-focused=false, which is the kind
  // of reading that invents a missing focus ring out of thin air.
  await control.click();
  await page.waitForTimeout(300);
  const focused = await read();

  // The control stays bare. Anything here is a SECOND edge over the wrapper's.
  expect(
    focused.controlOutline,
    "the control draws no outline -- the wrapper owns the ring, and an outline here is the extra edge reported twice",
  ).toBe(0);
  expect(focused.controlBorder, "and still no border").toBe(0);

  // ...and focus IS visible, on the WHOLE shadow string. Comparing slices is
  // what produced the second regression: the halo is the last component, so a
  // truncation drops exactly the part that differs and two clipped strings
  // compare equal.
  expect(
    focused.wrapperShadow,
    "the wrapper's ring changes on focus, so a keyboard user can see where they are",
  ).not.toBe(rest.wrapperShadow);

  // Specifically the accent ring, not merely the hover tint. The hover
  // selectors carry five class-level components and outrank the focus rules,
  // so with a pointer still over the field a broken guard leaves the hover
  // colour in place and "something changed" would pass while the ring is
  // absent.
  expect(
    focused.wrapperShadow,
    "the ring is the focus ring (a 3px halo), not the hover hairline",
  ).toMatch(/0px 0px 0px 3px/);
});
