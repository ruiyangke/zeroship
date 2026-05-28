import { chromium } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";

const baseUrl = process.env.STORYBOOK_URL;
if (!baseUrl) {
  throw new Error("Set STORYBOOK_URL to the running static Storybook URL.");
}

/*
 * Themes and story IDs grow as components land. Each new component
 * appends its story IDs here; each new palette appends a theme entry.
 */
const themes = [
  { label: "Crystal", value: "crystal" },
];
const stories = [
  "components-button--all-styles",
  "components-button--all-sizes",
  "components-button--all-states",
  "components-button--destructive",
  "components-button--destructive-disabled",
  "components-button--loading-destructive",
  "components-button--with-slots",
  "components-button--long-label",
  "components-button--focus-visible",
  "components-button--as-child",
  "components-input--all-variants",
  "components-input--all-sizes",
  "components-input--all-states",
  "components-input--with-slots",
  "components-input--decomposed",
  "components-input--combined",
  "components-input--required",
  "components-input--input-types",
  "components-input--horizontal-layout",
  "components-input--rtl",
  "components-input--long-label-and-description",
  "components-input--inside-form",
  // slice-2 review-fix additions (item 24)
  "components-input--field-size-inheritance",
  "components-input--error-boolean-only",
  "components-input--with-external-description",
  "components-input--field-disabled-propagation",
  "components-input--horizontal-layout-with-error",
  "components-input--input-ref-integration",
  "components-input--autofill",
  "components-input--custom-validate",
  // slice 3: Card / Dialog / AlertDialog.
  "components-card--all-variants",
  "components-card--all-sizes",
  "components-card--decomposed",
  "components-card--with-media",
  "components-card--media-sides",
  "components-card--interactive",
  "components-card--interactive-with-keyboard",
  "components-card--interactive-without-on-click",
  "components-card--as-child",
  "components-card--as-child-ref-composition",
  "components-card--ghost",
  "components-card--with-form-inside",
  "components-dialog--default",
  "components-dialog--sizes",
  "components-dialog--placement-top",
  "components-dialog--backdrop-tints",
  "components-dialog--with-form",
  "components-dialog--non-dismissible",
  "components-dialog--initial-focus",
  "components-dialog--nested",
  // Phase 2.B Dialog review-fix additions (item 21).
  "components-dialog--close-with-save-on-click",
  "components-dialog--close-as-child",
  "components-dialog--rtl",
  "components-dialog--long-footer-labels",
  // UnlabeledPopupWarns is intentionally missing a label (negative
  // test for the dev-warn) — its meta sets `a11y: { disable: true }`
  // so axe never runs against it. We keep it out of the a11y list
  // entirely to avoid spurious failures.
  "components-dialog--non-modal",
  "components-alertdialog--one-button",
  "components-alertdialog--two-buttons",
  "components-alertdialog--destructive",
  "components-alertdialog--three-buttons",
  "components-alertdialog--with-body",
  "components-alertdialog--outside-click-ignored",
  // Phase 2.C AlertDialog review-fix additions (items 1, 2, 5, 7).
  "components-alertdialog--esc-closes-cancel",
  "components-alertdialog--esc-no-ops-without-cancel",
  "components-alertdialog--esc-ignores-disabled-cancel",
  "components-alertdialog--cancel-with-cleanup-on-click",
  "components-alertdialog--cancel-as-child",
  "components-alertdialog--three-buttons-destructive-bottom",
  // Negative-test stories — excluded from the a11y list since their
  // story meta sets `a11y: { disable: true }` and they exist purely to
  // exercise dev-warn paths:
  //   components-alertdialog--three-buttons-destructive-misplaced
  //   components-alertdialog--destructive-without-cancel-warns
  // Slice 4: Checkbox / Switch / Radio (selection primitives).
  // Slice-4 visual-polish additions:
  //   - WithLabel → WithExternalLabel (item 6 rename) for Checkbox + Switch.
  //   - `Inline` (item 6 new story) for Checkbox + Switch.
  //   - `RequiredInvalid` (item 1 new story, auto-submits on mount) for
  //     Checkbox + Radio so a screenshot reviewer sees the post-submit
  //     red-error visual evidence the original Required story lacked.
  "components-checkbox--all-states",
  "components-checkbox--all-sizes",
  "components-checkbox--all-variants",
  "components-checkbox--with-external-label",
  "components-checkbox--inline",
  "components-checkbox--with-description",
  "components-checkbox--required",
  "components-checkbox--required-invalid",
  "components-checkbox--indeterminate-parent",
  "components-checkbox--indeterminate-from-group",
  "components-checkbox--inside-form",
  "components-checkbox--disabled",
  "components-checkbox--rtl",
  "components-switch--all-states",
  "components-switch--all-sizes",
  "components-switch--with-external-label",
  "components-switch--inline",
  "components-switch--with-description",
  "components-switch--immediate-effect",
  "components-switch--inside-form",
  "components-switch--disabled",
  "components-switch--rtl",
  "components-radio--two-options",
  "components-radio--five-options",
  "components-radio--horizontal",
  "components-radio--all-sizes",
  "components-radio--with-label",
  "components-radio--with-description",
  "components-radio--required",
  "components-radio--required-invalid",
  "components-radio--disabled",
  "components-radio--disabled-item",
  "components-radio--rtl",
];

if (themes.length === 0 || stories.length === 0) {
  console.log("A11y check skipped — no themes or stories registered yet (rebuild in progress).");
  process.exit(0);
}

const browser = await chromium.launch();
const context = await browser.newContext({ viewport: { width: 1280, height: 900 } });
const page = await context.newPage();
const failures = [];

for (const theme of themes) {
  for (const storyId of stories) {
    const themeGlobal = encodeURIComponent(theme.label);
    const url = `${baseUrl}/iframe.html?id=${storyId}&globals=theme:${themeGlobal}`;
    await page.goto(url, { waitUntil: "networkidle" });
    const results = await new AxeBuilder({ page }).analyze();
    const blockingViolations = results.violations.filter((violation) =>
      violation.impact === "serious" || violation.impact === "critical"
    );
    if (blockingViolations.length > 0) {
      failures.push({ theme: theme.value, storyId, violations: blockingViolations });
    }
  }
}

await context.close();
await browser.close();

if (failures.length > 0) {
  for (const failure of failures) {
    console.error(`A11y violations for ${failure.storyId} (${failure.theme})`);
    for (const violation of failure.violations) {
      console.error(`- ${violation.id}: ${violation.help}`);
    }
  }
  process.exit(1);
}

console.log(`A11y clean for ${stories.length} stories across ${themes.length} themes (no serious/critical violations).`);
