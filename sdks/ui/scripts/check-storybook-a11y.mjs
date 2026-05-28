import { chromium, webkit } from "@playwright/test";
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
  // Slice 5: Toggle + Toggle.Group (segmented control). Sixteen stories
  // mirroring the standalone + group axes: state grid, sizes, variants,
  // icon-only / icon+text, group two/five/multiple, sizes (group),
  // orientation (horizontal/vertical), equalWidth=false, Field-wrap,
  // disabled group, RTL.
  "components-toggle--all-states",
  "components-toggle--all-sizes",
  "components-toggle--all-variants",
  "components-toggle--with-icon-only",
  "components-toggle--with-icon-and-text",
  "components-toggle--disabled",
  "components-toggle--two-segments-single",
  "components-toggle--five-segments-single",
  "components-toggle--multiple-mode",
  "components-toggle--all-sizes-group",
  "components-toggle--horizontal",
  "components-toggle--vertical",
  "components-toggle--equal-width-off",
  "components-toggle--with-label",
  "components-toggle--disabled-group",
  "components-toggle--rtl",
  // Slice-5 review-fix regression targets (items 1 + 2).
  "components-toggle--forced-colors-hover",
  "components-toggle--role-toolbar-lock",
  // Slice 6: Select / Combobox / Autocomplete.
  // Count is 154, not the brief's 153: Autocomplete needed a new
  // Required baseline story plus its RequiredInvalid companion.
  "components-select--basic",
  "components-select--with-groups",
  "components-select--all-sizes",
  "components-select--all-variants",
  "components-select--multiple",
  "components-select--disabled",
  "components-select--with-label",
  "components-select--required",
  "components-select--required-invalid",
  "components-select--long-list",
  "components-select--align-start-center-end",
  "components-select--placement",
  "components-select--rtl",
  "components-combobox--basic",
  "components-combobox--multiple",
  "components-combobox--all-sizes",
  "components-combobox--all-variants",
  "components-combobox--empty",
  "components-combobox--with-label",
  "components-combobox--required",
  "components-combobox--required-invalid",
  "components-combobox--long-list",
  "components-combobox--disabled",
  "components-combobox--rtl",
  "components-combobox--aria-propagation",
  "components-autocomplete--basic",
  "components-autocomplete--all-sizes",
  "components-autocomplete--with-label",
  "components-autocomplete--empty",
  "components-autocomplete--disabled",
  "components-autocomplete--rtl",
  "components-autocomplete--with-description",
  "components-autocomplete--long-list",
  "components-autocomplete--required",
  "components-autocomplete--required-invalid",
  "components-autocomplete--aria-propagation",
  // Slice 7: NumberField + Slider. 10 stories each = 20 new entries.
  "components-numberfield--basic",
  "components-numberfield--all-sizes",
  "components-numberfield--all-variants",
  "components-numberfield--min-max-step",
  "components-numberfield--currency",
  "components-numberfield--scrub-area",
  "components-numberfield--disabled",
  "components-numberfield--with-label",
  "components-numberfield--required",
  "components-numberfield--aria-propagation",
  "components-numberfield--bare-focus",
  "components-numberfield--forced-colors-hover",
  "components-numberfield--coarse-pointer",
  "components-numberfield--rtl",
  "components-slider--basic",
  "components-slider--range",
  "components-slider--all-sizes",
  "components-slider--all-variants",
  "components-slider--steps",
  "components-slider--with-value",
  "components-slider--disabled",
  "components-slider--with-label",
  "components-slider--vertical",
  "components-slider--aria-propagation",
  "components-slider--forced-colors-outline",
  "components-slider--coarse-pointer",
  "components-slider--rtl",
  // Slice 10: Popover + Tooltip. 10 Popover + 8 Tooltip = 18 new entries.
  "components-popover--basic",
  "components-popover--with-title-description",
  "components-popover--with-arrow",
  "components-popover--with-backdrop",
  "components-popover--with-close",
  "components-popover--placement-side",
  "components-popover--align-start-center-end",
  "components-popover--nested-in-dialog",
  "components-popover--disabled",
  "components-popover--rtl",
  "components-tooltip--basic",
  "components-tooltip--with-delay",
  "components-tooltip--with-arrow",
  "components-tooltip--placement-side",
  "components-tooltip--on-focusable",
  "components-tooltip--rich-content",
  "components-tooltip--disabled",
  "components-tooltip--rtl",
  // Slice 8: Form + Fieldset (structural wrappers).
  "components-form--basic-submit",
  "components-form--with-validation",
  "components-form--validation-modes",
  "components-form--variants",
  "components-form--with-fields",
  "components-form--actions-ref-validate",
  "components-form--disabled",
  "components-form--rtl",
  "components-fieldset--basic-with-legend",
  "components-fieldset--all-sizes",
  "components-fieldset--nested-fields",
  "components-fieldset--disabled-cascade",
  "components-fieldset--with-form-integration",
  "components-fieldset--nested-fieldset",
  "components-fieldset--custom-legend-position",
  "components-fieldset--rtl",
  // Slice 9: OtpField + Meter + Progress. 22 new stories total.
  "components-otpfield--basic",
  "components-otpfield--custom-length",
  "components-otpfield--all-sizes",
  "components-otpfield--all-variants",
  "components-otpfield--with-label",
  "components-otpfield--required-invalid",
  "components-otpfield--disabled",
  "components-otpfield--rtl",
  "components-meter--basic",
  "components-meter--all-intents",
  "components-meter--all-sizes",
  "components-meter--with-value",
  "components-meter--ranges",
  "components-meter--disabled",
  "components-meter--rtl",
  "components-progress--determinate",
  "components-progress--indeterminate",
  "components-progress--all-sizes",
  "components-progress--with-value",
  "components-progress--completion-celebrate",
  "components-progress--with-label",
  "components-progress--rtl",
  // Slice 11: Menu + ContextMenu. 12 Menu + 6 ContextMenu = 18 new entries.
  "components-menu--basic-items",
  "components-menu--with-groups",
  "components-menu--with-separator",
  "components-menu--with-checkbox-item",
  "components-menu--with-radio-group",
  "components-menu--with-icons",
  "components-menu--with-keyboard-shortcuts",
  "components-menu--nested-submenu",
  "components-menu--with-arrow",
  "components-menu--disabled-item",
  "components-menu--placement-side",
  "components-menu--rtl",
  "components-contextmenu--basic-right-click-area",
  "components-contextmenu--with-checkbox-item",
  "components-contextmenu--nested-submenu",
  "components-contextmenu--with-disabled-item",
  "components-contextmenu--custom-anchor",
  "components-contextmenu--rtl",
];

async function launchBrowser() {
  try {
    return await chromium.launch();
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    if (!/sandbox_host_linux|crashpad/.test(message)) {
      throw error;
    }
    console.warn("Chromium launch failed in this sandbox; falling back to WebKit.");
    return webkit.launch();
  }
}

if (themes.length === 0 || stories.length === 0) {
  console.log("A11y check skipped — no themes or stories registered yet (rebuild in progress).");
  process.exit(0);
}

const browser = await launchBrowser();
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
