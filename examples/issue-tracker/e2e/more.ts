import type { Page } from "@playwright/test";

/**
 * Open the bug page's folded panels.
 *
 * Flags, votes and security sit behind a disclosure: on a typical bug all
 * three report absence, and cards saying "nothing here" directly under the
 * conversation are grid carrying no information.
 *
 * Keywords, duplicates and see-also USED to live here. They are rail groups
 * now (openRailGroup), because they are state rather than narrative and the
 * fold buried them under the conversation.
 *
 * Specs that drive those panels have to open it, exactly as a person does.
 * That is the honest cost of the change and it belongs in one helper rather
 * than copied into each spec -- when the fold changes again, this is the only
 * place that knows about it.
 */
export async function openMorePanels(page: Page): Promise<void> {
  const toggle = page.getByRole("button", { name: /flags, votes and security/i });
  // WAIT for it. `page.goto` resolves before React has rendered, so an
  // immediate count() would be 0 and an early return would quietly do nothing
  // -- the spec then fails several assertions later on a hidden panel, which
  // reads as "the panel is broken" rather than "the helper ran too soon".
  await toggle.waitFor({ state: "visible" });
  if ((await toggle.getAttribute("aria-expanded")) === "true") return;
  await toggle.click();
  await page.locator(".bug-detail-more section").first().waitFor({ state: "visible" });
}

/**
 * Open one of the rail's collapsed groups (Labels, CC, Dependencies,
 * Duplicates, See also).
 *
 * They rest as a single line -- "CC  --  [Add]" -- because the last time
 * these panels lived in the rail they rendered list and form at all times and
 * measured 1856px in a 352px column. The affordance is named per group so a
 * spec says which one it means.
 */
export async function openRailGroup(page: Page, label: string): Promise<void> {
  const toggle = page.getByRole("button", { name: new RegExp(`(Edit|Add) ${label}$`, "i") });
  await toggle.waitFor({ state: "visible" });
  if ((await toggle.getAttribute("aria-expanded")) === "true") return;
  await toggle.click();
  await page.locator(".rail-disclosure.is-open .rail-disclosure-body").first().waitFor({ state: "visible" });
}
