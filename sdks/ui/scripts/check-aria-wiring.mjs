/*
 * Verify the two new aria-wiring stories actually behave as advertised:
 *
 *   - WithExternalDescription: rendered <input> has aria-describedby
 *     containing BOTH the external id AND Field's auto-wired description id.
 *
 *   - InputRefIntegration: clicking the Focus button moves the focus to the
 *     input (proves the consumer-supplied ref reaches the node alongside
 *     Base UI's internal ref).
 *
 * Slice-2 review fix verification step 7. Prints pass/fail and exits
 * non-zero if either assertion fails.
 */
import { chromium } from "@playwright/test";

const baseUrl = process.env.STORYBOOK_URL;
if (!baseUrl) throw new Error("Set STORYBOOK_URL.");

const browser = await chromium.launch();
const ctx = await browser.newContext({ viewport: { width: 1280, height: 900 } });
const page = await ctx.newPage();

let failures = 0;

// ── 1. WithExternalDescription ────────────────────────────────────────
await page.goto(
  `${baseUrl}/iframe.html?id=components-input--with-external-description&globals=theme:Crystal`,
  { waitUntil: "networkidle" },
);
const input = page.locator('[data-testid="input-with-external-aria"]');
await input.waitFor({ state: "visible", timeout: 5000 });
const describedBy = (await input.getAttribute("aria-describedby")) ?? "";
const ids = describedBy.split(/\s+/).filter(Boolean);

const hasExternal = ids.includes("external-help");
// Field auto-generates an id; we just verify there's at least one MORE id
// beyond the external one. Base UI prefixes ids with `base-ui-` so check
// for any non-external id.
const hasFieldDescription = ids.some((id) => id !== "external-help" && id.length > 0);

const externalOk = hasExternal && hasFieldDescription && ids.length >= 2;
console.log(
  `[WithExternalDescription] aria-describedby="${describedBy}" → ${externalOk ? "PASS" : "FAIL"}`,
);
console.log(
  `  external id present: ${hasExternal}; field id present: ${hasFieldDescription}; ids: ${ids.length}`,
);
if (!externalOk) failures++;

// ── 2. InputRefIntegration ────────────────────────────────────────────
await page.goto(
  `${baseUrl}/iframe.html?id=components-input--input-ref-integration&globals=theme:Crystal`,
  { waitUntil: "networkidle" },
);
const focusBtn = page.getByRole("button", { name: "Focus input" });
const refInput = page.locator('[data-testid="input-ref-target"]');
await focusBtn.waitFor({ state: "visible", timeout: 5000 });
await focusBtn.click();
const statusText = (await page.locator('[data-testid="input-ref-status"]').innerText()).trim();
const isFocused = await refInput.evaluate((el) => el === document.activeElement);

const refOk = isFocused && statusText.includes("focused");
console.log(
  `[InputRefIntegration] status="${statusText}"; document.activeElement === input: ${isFocused} → ${refOk ? "PASS" : "FAIL"}`,
);
if (!refOk) failures++;

await ctx.close();
await browser.close();

if (failures > 0) {
  console.error(`\nARIA wiring assertions FAILED (${failures}).`);
  process.exit(1);
}
console.log("\nARIA wiring assertions PASSED (2/2).");
