import type { Page } from "@playwright/test";

/**
 * Open the bug page's folded panels.
 *
 * Keywords, flags, votes, duplicates, see-also and security sit behind a
 * disclosure: on a typical bug all six report absence, and six cards saying
 * "nothing here" directly under the conversation is four hundred pixels of
 * grid carrying no information.
 *
 * Specs that drive those panels have to open it, exactly as a person does.
 * That is the honest cost of the change and it belongs in one helper rather
 * than copied into each spec -- when the fold changes again, this is the only
 * place that knows about it.
 */
export async function openMorePanels(page: Page): Promise<void> {
  const toggle = page.getByRole("button", { name: /keywords, flags, votes/i });
  // WAIT for it. `page.goto` resolves before React has rendered, so an
  // immediate count() would be 0 and an early return would quietly do nothing
  // -- the spec then fails several assertions later on a hidden panel, which
  // reads as "the panel is broken" rather than "the helper ran too soon".
  await toggle.waitFor({ state: "visible" });
  if ((await toggle.getAttribute("aria-expanded")) === "true") return;
  await toggle.click();
  await page.locator(".bug-detail-more section").first().waitFor({ state: "visible" });
}
