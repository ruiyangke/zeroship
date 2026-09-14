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
  { label: "Crystal Light", value: "crystal-light" },
  { label: "Crystal Dark", value: "crystal-dark" },
  { label: "Studio Light", value: "studio-light" },
  { label: "Ghibli Light", value: "ghibli-light" },
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
  "components-button--as-child-busy-and-disabled",
  "components-button--as-child-disabled-is-inert",
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
  // wave-7 🔴 2 + 🟡 4: aria-disabled keyboard suppression and Title
  // asChild single ref attach.
  "components-card--interactive-aria-disabled",
  "components-card--title-as-child-single-attach",
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
  // wave-7 focused-review 🔴 #2 addition (forced-colors popup mirror).
  "components-alertdialog--forced-colors",
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
  // Wave-7 red #1 regression — RTL glyph centering. Renders a checked
  // + an indeterminate chip under dir="rtl" so the aria-wiring script
  // can measure the glyph stays centered inside the chip. The story
  // also needs to be axe-clean.
  "components-checkbox--rtl-glyph-centering",
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
  // ToggleGroup focused-review 🔴 regressions (items 1 + 2). The
  // Field.Label wiring regression rides the existing `with-label`
  // story above (re-wired to use `aria-labelledby` instead of the
  // masking `aria-label`).
  "components-toggle--controlled-clearable",
  "components-toggle--role-override-attempt",
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
  // Wave-8 review-fix 🔴 #1 / 🔴 #2 regression stories.
  "components-select--field-aria-autowiring",
  "components-select--aria-described-by-merge",
  "components-select--invalid-focus-ring",
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
  // Wave-10 review-fix 🔴 #A / 🔴 #B regression stories.
  "components-autocomplete--field-aria-autowiring",
  "components-autocomplete--placeholder-fallback",
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
  "components-numberfield--field-auto-aria",
  "components-numberfield--consumer-focus-handlers",
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
  "components-slider--consumer-style-preserved",
  "components-slider--required-cascade",
  "components-slider--range-labelled-by",
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
  "components-tooltip--detached-handle",
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
  "components-fieldset--aria-labelled-by-undefined",
  "components-fieldset--nested-fieldset-disabled-cascade",
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
  "components-meter--external-aria-labelling",
  "components-meter--rtl",
  "components-progress--determinate",
  "components-progress--indeterminate",
  "components-progress--all-sizes",
  "components-progress--with-value",
  "components-progress--completion-celebrate",
  // wave10 fix: ExternalAriaLabelling dropped showValue to suppress the
  // misleading "7%" Base UI percent badge on a custom-range progress;
  // including the story in the a11y sweep keeps that path covered.
  "components-progress--external-aria-labelling",
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
  // Slice 11 review-fix (wave9 🔴 2 regression): the new LinkItem
  // single-ref-attach story and the existing asChild LinkItem story.
  "components-menu--with-link-item-as-child",
  "components-menu--link-item-as-child-single-attach",
  "components-contextmenu--basic-right-click-area",
  "components-contextmenu--with-checkbox-item",
  "components-contextmenu--nested-submenu",
  "components-contextmenu--with-disabled-item",
  "components-contextmenu--custom-anchor",
  "components-contextmenu--rtl",
  "components-contextmenu--positioned-popup-override",
  "components-contextmenu--disabled-trigger",
  "components-contextmenu--as-child",
  // Slice 13: Tabs. 12 stories.
  "components-tabs--basic",
  "components-tabs--all-variants",
  "components-tabs--all-sizes",
  "components-tabs--vertical",
  "components-tabs--with-icons",
  "components-tabs--with-badges",
  "components-tabs--many-tabs",
  "components-tabs--disabled-tab",
  "components-tabs--controlled-value",
  "components-tabs--with-animated-indicator",
  "components-tabs--lazy-mount-panel",
  "components-tabs--rtl",
  // slice 12: Menubar / Toolbar / NavigationMenu.
  "components-menubar--basic",
  "components-menubar--with-submenus",
  "components-menubar--with-checkbox-item",
  "components-menubar--with-radio-group",
  "components-menubar--keyboard-nav",
  "components-menubar--disabled",
  "components-menubar--with-icons",
  "components-menubar--role-lock-regression",
  "components-menubar--render-injection-regression",
  "components-menubar--non-modal-default-regression",
  "components-menubar--aria-orientation-lock-regression",
  "components-menubar--rtl",
  "components-toolbar--basic",
  "components-toolbar--with-separator",
  "components-toolbar--with-toggle-group",
  "components-toolbar--with-icon-buttons",
  "components-toolbar--vertical",
  "components-toolbar--with-groups",
  "components-toolbar--disabled",
  "components-toolbar--rtl",
  "components-toolbar--role-lock-regression",
  "components-toolbar--roving",
  "components-toolbar--separator-orientation-lock-regression",
  "components-navigationmenu--basic",
  "components-navigationmenu--with-content",
  "components-navigationmenu--with-icons",
  "components-navigationmenu--with-viewport",
  "components-navigationmenu--with-arrow",
  "components-navigationmenu--keyboard-nav",
  "components-navigationmenu--disabled",
  "components-navigationmenu--rtl",
  // Wave 10 review regressions:
  "components-navigationmenu--as-child-ref-attach-regression",
  "components-navigationmenu--icon-rotation-regression",
  "components-navigationmenu--popup-min-width-clamp-regression",
  // Slice 14: Accordion + Collapsible.
  "components-accordion--basic",
  "components-accordion--multiple-open",
  "components-accordion--collapsible",
  "components-accordion--controlled",
  "components-accordion--with-default-value",
  "components-accordion--disabled",
  "components-accordion--horizontal",
  "components-accordion--regression-horizontal-transition",
  "components-accordion--rtl",
  "components-accordion--rich-content",
  "components-collapsible--basic",
  "components-collapsible--controlled",
  "components-collapsible--disabled",
  "components-collapsible--inside-card",
  "components-collapsible--rtl",
  // Wave 10 fix #1: asChild surface on Trigger + Panel via _slot.ts.
  "components-collapsible--trigger-as-child",
  "components-collapsible--panel-as-child",
  // Wave 10 rework fix #2: onOpenChange forwards Base UI details.
  "components-collapsible--regression-on-open-change-details",
  // Wave 10 rework fix #3: forced-colors disabled+open trigger.
  "components-collapsible--regression-disabled-open-forced-colors",
  // Slice 16: Toast. 15 stories covering basic / variant / position /
  // duration / update / stacked / swipe / RTL — the imperative
  // `useToast()` surface drives every story.
  "components-toast--basic",
  "components-toast--with-description",
  "components-toast--with-action",
  "components-toast--success",
  "components-toast--error-variant",
  "components-toast--warning",
  "components-toast--info",
  "components-toast--long-duration",
  "components-toast--persistent",
  "components-toast--imperative-update",
  "components-toast--stacked",
  "components-toast--position-top",
  "components-toast--position-bottom",
  "components-toast--swipe-to-dismiss",
  "components-toast--rtl",
  // Wave 8 review fix: Toast.Close keeps default `aria-label` when a
  // consumer spreads `aria-label={undefined}` AND never carries
  // `aria-hidden="true"` at rest (Base UI parks it there until viewport
  // expansion; we override to undefined). Story exists primarily for
  // the aria-wiring regression but is axe-clean on its own merits.
  "components-toast--close-label-default",
  // Slice 15: Drawer — 12 stories (review-fix 8 added SizesVertical).
  "components-drawer--basic",
  "components-drawer--left-side",
  "components-drawer--top",
  "components-drawer--bottom",
  "components-drawer--sizes",
  "components-drawer--sizes-vertical",
  "components-drawer--controlled",
  "components-drawer--with-form",
  "components-drawer--with-long-content",
  "components-drawer--nested",
  "components-drawer--rtl",
  "components-drawer--close-as-child",
  // Slice 18: ScrollArea — 10 stories.
  "components-scrollarea--basic-vertical",
  "components-scrollarea--basic-horizontal",
  "components-scrollarea--both",
  "components-scrollarea--always-visible",
  "components-scrollarea--hover-only",
  "components-scrollarea--long-list",
  "components-scrollarea--grid-content",
  "components-scrollarea--inside-card",
  "components-scrollarea--rtl",
  "components-scrollarea--keyboard-scroll",
  // Slice 17: Avatar + Separator.
  "components-avatar--basic",
  "components-avatar--fallback",
  "components-avatar--fallback-on-error",
  // Wave 9 review-fix 🔴: decorative-image + identity-image aria-wiring
  // for the substitute-for-image Fallback states.
  "components-avatar--decorative-fallback",
  "components-avatar--fallback-delay",
  "components-avatar--sizes",
  "components-avatar--shapes",
  "components-avatar--with-icon",
  "components-avatar--group",
  "components-avatar--rtl",
  "components-separator--horizontal",
  "components-separator--vertical",
  "components-separator--hairline",
  "components-separator--thick",
  "components-separator--not-decorative",
  "components-separator--inside-list",
  "components-separator--inside-toolbar",
  "components-separator--rtl",
  // Slice 19: PreviewCard. 9 stories — hover-anchored rich preview
  // surface. Triggers render visible `<a>` / `<button>` so axe doesn't
  // need the popup mounted to validate the resting state.
  "components-previewcard--basic",
  "components-previewcard--user-handle",
  "components-previewcard--link-preview",
  "components-previewcard--long-content",
  "components-previewcard--as-child",
  "components-previewcard--with-arrow",
  "components-previewcard--placement-side",
  "components-previewcard--rtl",
  "components-previewcard--detached-handle",
  // Slice 20: CheckboxGroup. Layout-only primitive; axe walks each
  // story's chip + label cluster.
  "components-checkboxgroup--basic",
  "components-checkboxgroup--controlled",
  "components-checkboxgroup--disabled",
  "components-checkboxgroup--horizontal",
  "components-checkboxgroup--nested-fieldset",
  "components-checkboxgroup--rtl",
  "components-checkboxgroup--all-selected",
  "components-checkboxgroup--none-selected",
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

/*
 * Per-story axe rule disables. Some stories must leave the popup open
 * for their regression to read computed styles. Base UI's
 * `data-base-ui-focus-guard` spans (`tabindex=0 aria-hidden=true`) are
 * part of floating-ui's focus trap and not part of our public surface;
 * we disable just the `aria-hidden-focus` rule for those stories.
 *
 * Mirrors the per-story `parameters.a11y.config.rules` knob the Test
 * Runner reads via `getStoryContext` — but `check-storybook-a11y.mjs`
 * uses AxeBuilder directly and cannot reach the story context, so the
 * disable list lives here. Keep this list short and document each
 * entry.
 */
const ruleDisables = {
  // Wave-8 🔴 #2 regression: needs `[data-popup-open]` to read the
  // trigger's computed focus-ring slot.
  "components-select--invalid-focus-ring": ["aria-hidden-focus"],
};

for (const theme of themes) {
  for (const storyId of stories) {
    const themeGlobal = encodeURIComponent(theme.label);
    const url = `${baseUrl}/iframe.html?id=${storyId}&globals=theme:${themeGlobal}`;
    await page.goto(url, { waitUntil: "networkidle" });
    let builder = new AxeBuilder({ page });
    const disabledRules = ruleDisables[storyId];
    if (disabledRules && disabledRules.length > 0) {
      builder = builder.disableRules(disabledRules);
    }
    const results = await builder.analyze();
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
