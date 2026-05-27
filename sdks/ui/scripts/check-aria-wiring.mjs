/*
 * Verify aria-wiring and behavior contracts across slices.
 *
 * Slice 2 assertions (Input):
 *   - WithExternalDescription: aria-describedby contains BOTH external id
 *     AND Field's auto-wired description id.
 *   - InputRefIntegration: clicking the Focus button moves focus to the input.
 *
 * Slice 3 assertions (Dialog + AlertDialog):
 *   - Dialog popup carries role="dialog", aria-labelledby refs Title id,
 *     aria-describedby refs Description id (when present).
 *   - AlertDialog popup carries role="alertdialog", same labelling.
 *   - Focus is trapped — Tab from the last focusable cycles back; Shift+Tab
 *     from the first cycles to the last.
 *   - ESC closes a dismissible Dialog; ESC does NOT close a non-dismissible Dialog.
 *   - AlertDialog: outside-click does NOT dismiss; ESC closes Cancel (so the
 *     dialog closes when Cancel is present).
 *   - Focus is restored to the Trigger on close.
 *
 * Prints pass/fail per assertion; exits non-zero if any failed.
 */
import { chromium } from "@playwright/test";

const baseUrl = process.env.STORYBOOK_URL;
if (!baseUrl) throw new Error("Set STORYBOOK_URL.");

const browser = await chromium.launch();
const ctx = await browser.newContext({ viewport: { width: 1280, height: 900 } });
const page = await ctx.newPage();

let failures = 0;
function report(label, ok, extra) {
  console.log(`[${label}] ${ok ? "PASS" : "FAIL"}${extra ? ` — ${extra}` : ""}`);
  if (!ok) failures++;
}

async function open(storyId) {
  await page.goto(
    `${baseUrl}/iframe.html?id=${storyId}&globals=theme:Crystal`,
    { waitUntil: "networkidle" },
  );
}

/* ─── 1. Input WithExternalDescription (slice 2) ────────────────────── */
await open("components-input--with-external-description");
{
  const input = page.locator('[data-testid="input-with-external-aria"]');
  await input.waitFor({ state: "visible", timeout: 5000 });
  const describedBy = (await input.getAttribute("aria-describedby")) ?? "";
  const ids = describedBy.split(/\s+/).filter(Boolean);
  const hasExternal = ids.includes("external-help");
  const hasFieldDescription = ids.some((id) => id !== "external-help" && id.length > 0);
  const ok = hasExternal && hasFieldDescription && ids.length >= 2;
  report(
    "Input WithExternalDescription",
    ok,
    `aria-describedby="${describedBy}" (external=${hasExternal}, field=${hasFieldDescription}, count=${ids.length})`,
  );
}

/* ─── 2. Input ref integration (slice 2) ────────────────────────────── */
await open("components-input--input-ref-integration");
{
  const focusBtn = page.getByRole("button", { name: "Focus input" });
  const refInput = page.locator('[data-testid="input-ref-target"]');
  await focusBtn.waitFor({ state: "visible", timeout: 5000 });
  await focusBtn.click();
  const statusText = (await page.locator('[data-testid="input-ref-status"]').innerText()).trim();
  const isFocused = await refInput.evaluate((el) => el === document.activeElement);
  const ok = isFocused && statusText.includes("focused");
  report("Input ref integration", ok, `status="${statusText}", activeElement matches: ${isFocused}`);
}

/* ─── 3. Dialog default: role + aria-labelledby + aria-describedby ──── */
await open("components-dialog--default");
{
  const popup = page.locator('[data-testid="dialog-default-popup"]');
  await popup.waitFor({ state: "visible", timeout: 5000 });
  const role = await popup.getAttribute("role");
  const labelledBy = await popup.getAttribute("aria-labelledby");
  const describedBy = await popup.getAttribute("aria-describedby");
  // Using [id="..."] avoids CSS.escape (unavailable in Node) and quotes
  // colon/special chars Base UI's ids can contain.
  const title = labelledBy ? page.locator(`[id="${labelledBy}"]`) : null;
  const desc = describedBy ? page.locator(`[id="${describedBy}"]`) : null;
  const titleText = title ? (await title.innerText().catch(() => "")).trim() : "";
  const descText = desc ? (await desc.innerText().catch(() => "")).trim() : "";
  const ok =
    role === "dialog" &&
    Boolean(labelledBy) &&
    Boolean(describedBy) &&
    titleText.length > 0 &&
    descText.length > 0;
  report(
    "Dialog default ARIA",
    ok,
    `role=${role} labelledby=${labelledBy} (=> "${titleText}") describedby=${describedBy} (=> "${descText}")`,
  );
}

/* ─── 4. Dialog focus is trapped (focus wraps back into popup) ──────
 *
 * Base UI's focus trap uses sentinel focus-guards (`<span
 * data-base-ui-focus-guard>` at body level) that briefly hold focus
 * between cycles before bouncing it back into the popup. So asserting
 * activeElement at EVERY keystep is too strict — the assertion that
 * actually matters is that focus is RE-CAPTURED into the popup after
 * a wrap. We Tab from the last focusable and verify focus lands back
 * inside within a few keypresses; same for Shift+Tab from the first. */
await open("components-dialog--with-form");
{
  const popup = page.locator('[data-testid="dialog-with-form"]');
  await popup.waitFor({ state: "visible", timeout: 5000 });
  const focusables = await popup
    .locator(
      'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])',
    )
    .all();
  const count = focusables.length;
  if (count < 2) {
    report("Dialog focus trap", false, `not enough focusables (${count})`);
  } else {
    async function focusInsidePopup() {
      return page.evaluate(() => {
        const popup = document.querySelector('[data-testid="dialog-with-form"]');
        return (
          !!popup &&
          !!document.activeElement &&
          (popup === document.activeElement ||
            popup.contains(document.activeElement))
        );
      });
    }
    // Forward wrap: focus the last; Tab a couple of times; should land
    // back inside the popup (after passing through the guard sentinels).
    await focusables[count - 1].focus();
    let recapturedForward = false;
    for (let i = 0; i < 12; i++) {
      await page.keyboard.press("Tab");
      if (await focusInsidePopup()) {
        recapturedForward = true;
        break;
      }
    }
    // Backward wrap: focus the first; Shift+Tab; should land back inside.
    await focusables[0].focus();
    let recapturedBackward = false;
    for (let i = 0; i < 12; i++) {
      await page.keyboard.press("Shift+Tab");
      if (await focusInsidePopup()) {
        recapturedBackward = true;
        break;
      }
    }
    report(
      "Dialog focus trap (recaptured on wrap)",
      recapturedForward && recapturedBackward,
      `forward-recapture=${recapturedForward}, backward-recapture=${recapturedBackward}`,
    );
  }
}

/* ─── 5. Dialog ESC closes when dismissible ─────────────────────────── */
await open("components-dialog--with-form");
{
  const popup = page.locator('[data-testid="dialog-with-form"]');
  await popup.waitFor({ state: "visible", timeout: 5000 });
  await page.keyboard.press("Escape");
  // Allow the close animation to finish.
  await page.waitForTimeout(400);
  const isHidden = (await popup.count()) === 0 || !(await popup.first().isVisible().catch(() => false));
  report("Dialog ESC dismisses dismissible=true", isHidden);
}

/* ─── 6. Dialog ESC does NOT close when dismissible=false ───────────── */
await open("components-dialog--non-dismissible");
{
  const popup = page.locator('[data-testid="dialog-non-dismissible"]');
  await popup.waitFor({ state: "visible", timeout: 5000 });
  await page.keyboard.press("Escape");
  await page.waitForTimeout(400);
  const stillVisible = await popup.isVisible().catch(() => false);
  report("Dialog ESC ignored when dismissible=false", stillVisible);
}

/* ─── 7. AlertDialog role + labelling ───────────────────────────────── */
await open("components-alertdialog--two-buttons");
{
  const popup = page.locator('[data-testid="alertdialog-two-buttons"]');
  await popup.waitFor({ state: "visible", timeout: 5000 });
  const role = await popup.getAttribute("role");
  const labelledBy = await popup.getAttribute("aria-labelledby");
  const describedBy = await popup.getAttribute("aria-describedby");
  const ok =
    role === "alertdialog" && Boolean(labelledBy) && Boolean(describedBy);
  report(
    "AlertDialog ARIA",
    ok,
    `role=${role} labelledby=${labelledBy} describedby=${describedBy}`,
  );
}

/* ─── 8. AlertDialog outside-click does NOT dismiss ─────────────────── */
await open("components-alertdialog--outside-click-ignored");
{
  const popup = page.locator('[data-testid="alertdialog-outside-click"]');
  await popup.waitFor({ state: "visible", timeout: 5000 });
  // Click the backdrop at a position outside the popup (corner).
  await page.mouse.click(10, 10);
  await page.waitForTimeout(400);
  const stillVisible = await popup.isVisible().catch(() => false);
  report("AlertDialog outside-click ignored", stillVisible);
}

/* ─── 9. AlertDialog ESC closes (via Cancel) ────────────────────────── */
await open("components-alertdialog--two-buttons");
{
  const popup = page.locator('[data-testid="alertdialog-two-buttons"]');
  await popup.waitFor({ state: "visible", timeout: 5000 });
  await page.keyboard.press("Escape");
  await page.waitForTimeout(400);
  const isHidden = !(await popup.isVisible().catch(() => false));
  report("AlertDialog ESC closes", isHidden);
}

/* ─── 10. Dialog focus restore: trigger gets focus back on close ───── */
// Use the Sizes story: it has triggers that do NOT auto-open, so we
// can deterministically open + close + verify the trigger is focused.
await open("components-dialog--sizes");
{
  const trigger = page.getByRole("button", { name: "md" });
  await trigger.waitFor({ state: "visible", timeout: 5000 });
  await trigger.click();
  const popup = page.locator('[data-testid="dialog-size-md"]');
  await popup.waitFor({ state: "visible", timeout: 5000 });
  // Press ESC to close.
  await page.keyboard.press("Escape");
  await page.waitForTimeout(500);
  // Trigger should be the active element.
  const restored = await trigger.evaluate((el) => el === document.activeElement);
  report("Dialog focus restore on close", restored);
}

await ctx.close();
await browser.close();

if (failures > 0) {
  console.error(`\nARIA wiring assertions FAILED (${failures}).`);
  process.exit(1);
}
console.log("\nARIA wiring assertions PASSED.");
