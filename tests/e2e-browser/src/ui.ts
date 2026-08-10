// Small helpers for driving a demo UI that may be broken.
//
// The awkward part of testing "does this demo work" is that on a broken demo
// every Playwright call throws or hangs: the input never enables, the list item
// never appears. If each of those bubbles as its own error, the red you get is
// the FIRST thing that went wrong mechanically, not the thing you wanted to
// assert — and if enough of them stack up, the test dies on the runner's own
// timeout, which is the least informative red of all (it names no selector, no
// server error, nothing).
//
// So: interactions here report whether they worked instead of throwing, on a
// short leash, and the test then asserts on the outcome with the browser and
// server diagnostics attached. The result is a red that says "the task never
// appeared in the list, and here is the TypeError the server threw".

import type { Locator, Page } from "playwright";

/** How long to wait for the UI to reach a state before calling it "did not". */
export const UI_TIMEOUT = Number(process.env.ZEROSHIP_E2E_UI_TIMEOUT_MS ?? 10_000);
/** How long to wait for a single interaction (fill/click) to become possible. */
export const ACTION_TIMEOUT = Number(process.env.ZEROSHIP_E2E_ACTION_TIMEOUT_MS ?? 5_000);

/** Did `selector` show up? Never throws. */
export async function appeared(
  page: Page,
  selector: string,
  timeout = UI_TIMEOUT,
): Promise<boolean> {
  try {
    await page.waitForSelector(selector, { timeout, state: "attached" });
    return true;
  } catch {
    return false;
  }
}

/** Did typing into `locator` work? Never throws. */
export async function tryFill(
  locator: Locator,
  value: string,
  timeout = ACTION_TIMEOUT,
): Promise<boolean> {
  try {
    await locator.fill(value, { timeout });
    return true;
  } catch {
    return false;
  }
}

/** Did clicking `locator` work? Never throws. */
export async function tryClick(locator: Locator, timeout = ACTION_TIMEOUT): Promise<boolean> {
  try {
    await locator.click({ timeout });
    return true;
  } catch {
    return false;
  }
}

/** Did the input with this placeholder become enabled? Never throws. */
export async function inputEnabled(
  page: Page,
  placeholder: string,
  timeout = UI_TIMEOUT,
): Promise<boolean> {
  try {
    await page.waitForFunction(
      (ph: string) => {
        const el = document.querySelector<HTMLInputElement>(`input[placeholder="${ph}"]`);
        return el !== null && !el.disabled;
      },
      placeholder,
      { timeout },
    );
    return true;
  } catch {
    return false;
  }
}
