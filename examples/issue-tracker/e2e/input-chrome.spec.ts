import { expect, test } from "@playwright/test";

import { signIn } from "./session";

/**
 * A field draws ONE ring, not two.
 *
 * Reported as "for all the inputs, there are dual borders". The app styled
 * fields with bare element selectors:
 *
 *   input, select, textarea       { border: 1px solid ...; border-radius: 7px }
 *   input:focus, select:focus, .. { outline: 2px solid ...; border-color: ... }
 *
 * An element selector matches the design system's controls too. The DS draws
 * the field as a wrapper with a 1px inset ring and leaves the inner control
 * bare, so at rest its class beat the app rule and one ring showed. On focus
 * there was nothing to beat -- the DS sets no outline on the control -- so the
 * app's outline landed on top of the wrapper's ring: two rings, two greens.
 *
 * The app CSS had been cleaned of `.zs-*` selectors once. This was the same
 * trespass by ELEMENT name rather than class name, which that cleanup could
 * not see.
 *
 * Asserts the rule, not the pixels: no app stylesheet may style a bare field
 * element. A screenshot test would pass again the moment someone reintroduced
 * the selector with a different colour.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);

test("no stylesheet gives a design-system field its own border", async ({ page, baseURL }) => {
  await signIn(page.context(), { runtimePort: RUNTIME_PORT, baseURL: baseURL! });
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto("/#/bugs");

  const control = page.locator("input.zs-input__control").first();
  await expect(control).toBeVisible();

  // At rest the design system owns the field entirely.
  const rest = await control.evaluate((el) => {
    const cs = getComputedStyle(el);
    return { border: parseFloat(cs.borderTopWidth) || 0, outline: parseFloat(cs.outlineWidth) || 0 };
  });
  expect(rest.border, "the control carries no border of its own").toBe(0);
  expect(rest.outline, "and no outline at rest").toBe(0);

  // THE RULE, read out of the live stylesheets.
  //
  // Checking the computed style of a DS control cannot catch this: its class
  // beats a bare element selector, so `border` reads 0 whether or not the bad
  // rule exists. An earlier version of this spec did exactly that and passed
  // happily with the offending CSS pasted back in. What has to be asserted is
  // that no sheet TRIES.
  const offenders = await page.evaluate(() => {
    const bad: string[] = [];
    const bareField = /(^|[\s,>+~(])(input|textarea|select)(\b)(?![-\w])/i;
    for (const sheet of Array.from(document.styleSheets)) {
      let rules: CSSRule[];
      try {
        rules = Array.from(sheet.cssRules ?? []);
      } catch {
        continue; // cross-origin sheet; not ours
      }
      const walk = (list: CSSRule[]) => {
        for (const rule of list) {
          if ((rule as CSSGroupingRule).cssRules) walk(Array.from((rule as CSSGroupingRule).cssRules));
          const style = (rule as CSSStyleRule).style;
          const selector = (rule as CSSStyleRule).selectorText;
          if (!style || !selector) continue;
          if (!bareField.test(selector)) continue;
          // A border on a bare field element is the doubling: the design
          // system already draws the field, so this can only be a second edge.
          const border =
            style.getPropertyValue("border") ||
            style.getPropertyValue("border-width") ||
            style.getPropertyValue("border-color") ||
            style.getPropertyValue("border-top-width");
          if (border && border !== "0" && border !== "0px" && border !== "none") {
            bad.push(`${selector} { border: ${border} }`);
          }
        }
      };
      walk(rules);
    }
    return bad;
  });
  expect(
    offenders,
    `a stylesheet gives bare field elements a border, which the design system already draws:\n${offenders.join("\n")}`,
  ).toEqual([]);

  // And there IS still a visible focus ring, standing off the field.
  for (let i = 0; i < 12; i++) {
    await page.keyboard.press("Tab");
    if (
      await page.evaluate(() =>
        document.activeElement?.classList.contains("zs-input__control") ?? false,
      )
    ) {
      break;
    }
  }
  const focused = await page.evaluate(() => {
    const el = document.activeElement!;
    const cs = getComputedStyle(el);
    return {
      isField: el.classList.contains("zs-input__control"),
      outline: parseFloat(cs.outlineWidth) || 0,
      offset: parseFloat(cs.outlineOffset) || 0,
    };
  });
  expect(focused.isField, "a field took keyboard focus").toBe(true);
  expect(focused.outline, "there IS a visible focus ring (the DS supplies none)").toBeGreaterThan(0);
  expect(focused.offset, "and it stands off the field rather than doubling its edge").toBeGreaterThan(0);
});
