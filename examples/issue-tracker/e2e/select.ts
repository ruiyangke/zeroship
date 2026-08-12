import { expect, type Locator, type Page } from "@playwright/test";

/**
 * Choose an option from a `@zeroship/ui` Select.
 *
 * The design system's Select is a combobox, not a native `<select>`, so
 * `selectOption` does nothing on it -- it needs a click on the trigger and a
 * click on the option. The options render in a PORTAL at document body, so
 * they are not inside whatever locator scopes the trigger; querying them from
 * a scoped locator finds nothing and reads as "the select has no such option"
 * when the option is right there on screen.
 *
 * `scope` narrows the TRIGGER (a panel or section), `page` finds the OPTION.
 * They are separate arguments for that reason and not by accident.
 */
export async function chooseOption(
  page: Page,
  scope: Page | Locator,
  select: string | RegExp,
  option: string | RegExp,
): Promise<void> {
  const trigger = scope.getByRole("combobox", { name: select });
  await expect(trigger, `no select named ${String(select)}`).toBeVisible();
  await trigger.click();
  // Scoped to the OPEN listbox, not the whole page. A native <select>
  // elsewhere on the page keeps its <option> elements in the DOM at all times
  // and they carry role="option" too, so an unscoped query matched both the
  // portal item and a hidden native one -- a strict-mode violation that reads
  // as "the option is ambiguous" when only one of them is on screen.
  const item = page.getByRole("listbox").getByRole("option", { name: option });
  await expect(item, `no option named ${String(option)}`).toBeVisible();
  await item.click();
  // The popup closes on selection; waiting for it keeps a following click from
  // landing on the overlay instead of the control it was aimed at.
  await expect(item).toBeHidden();
}
