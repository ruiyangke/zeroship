# Slice 4 — Checkbox + Switch + Radio (selection primitives)

**Worktree** `.worktrees/ui-design` @ branch `builder/ui-design` (HEAD `31e3c806`).

Three components that share visual language (checkmark / track / dot) and integrate with `Field`. First slice of the forms slate; subsequent form slices reuse the size system, Field integration shape, and aria-wiring patterns established here.

## Goal

`@zeroship/ui` ships three selection primitives that:

- Wrap Base UI's `Checkbox`, `Switch`, and `Radio`/`RadioGroup` headless primitives (verified shapes at `@base-ui/react@1.5.0/{checkbox,switch,radio,radio-group}`).
- Compose with `Field` so `<Field><Field.Label>…</Field.Label><Checkbox /></Field>` carries the existing aria-wiring (htmlFor, aria-describedby, aria-invalid) automatically.
- Inherit `size` (`sm`/`md`/`lg`) and `disabled` from Field via the same context the Input/Field pair uses (`FieldVisualSize` + `FieldDisabledContext` from `Field.tsx`).
- Carry Crystal-theme styling: indigo accent fill when checked/on, label-mix border when unchecked, focus ring matching Button's `--zs-button-focus-ring-color` palette.
- Support a `tinted` variant for cases where the surrounding surface is loud (matches Input's `variant="tinted"`).

## Hard constraints (unchanged)

- Pre-launch, no back-compat.
- Plain CSS + `--zs-*` tokens. No Tailwind, no `@apply`.
- No raw hex, no raw px (incl. comments).
- HIG as principle but **not in source**.
- `prefers-reduced-motion`, `@media (forced-colors: active)`, RTL via logical properties.
- Glass-surface invariant: opaque `background-color` on every element that the user sees, then optional `backdrop-filter` for visuals.
- DO NOT commit, push, or merge.

## Principles (in source as design rationale, never branded)

These are the design guarantees the components encode — write them as code comments where they shape the implementation. **Do not mention "HIG" or "Apple".**

1. **Current state must be obvious.** Checked / unchecked / indeterminate / disabled / focused — five visual states per component, all distinguishable without color alone (shape + fill + ring, not just hue).
2. **Switch for binary state.** On / off. No third state. `defaultChecked` for forms, immediate-effect for settings.
3. **Checkbox for hierarchical or independent selections.** Single checkbox = one boolean. Group = multiple independent booleans. Indeterminate = parent of partially-checked children.
4. **Radio for mutually exclusive options.** 2–5 typical. Beyond 5 a `Select` is the better surface (later slice).
5. **Don't recolor the on-state by default.** The accent is the system signal — overriding it loses the system-control affordance. Consumers can override via CSS variable, but the default is `--zs-accent`.
6. **Hit target ≥ 1.75rem regardless of visual size.** The visual chip can be small; the tap/click rect cannot. Wrap visual + label in a single focusable surface.

## Component API

### Checkbox

```tsx
// sdks/ui/src/components/Checkbox/Checkbox.tsx
export interface CheckboxProps extends Omit<BaseCheckboxRootProps, "render"> {
  /** Visual size — small 1rem, medium 1.125rem (default), large 1.25rem. */
  size?: "sm" | "md" | "lg";
  /** Visual variant. `default` is the standard chip; `tinted` softens it for loud surfaces. */
  variant?: "default" | "tinted";
  /** Render-as a custom element. Composes via Slot (see _slot.ts). */
  asChild?: boolean;
  /** Class name for the visible chip (the <span> Base UI renders). */
  className?: string;
}

// Subparts: only the Indicator (the checkmark / minus glyph). Body is implicit.
export const Checkbox = ForwardedCheckbox as CheckboxComponent;
// Checkbox is a controlled-or-uncontrolled chip + indicator pair. No
// Checkbox.Indicator subpart — the checkmark is internal to the chip.
```

The visible chip is a `<span>` painting the box; Base UI renders the hidden `<input>` beside it for form submission and screen-reader semantics. The indicator (checkmark SVG when `checked`, minus SVG when `indeterminate`) lives inside the chip.

### Switch

```tsx
// sdks/ui/src/components/Switch/Switch.tsx
export interface SwitchProps extends Omit<BaseSwitchRootProps, "render"> {
  /** Visual size — small 1.5rem wide, medium 2rem (default), large 2.5rem. */
  size?: "sm" | "md" | "lg";
  /** Render-as a custom element. Composes via Slot. */
  asChild?: boolean;
  className?: string;
}

export const Switch = ForwardedSwitch as SwitchComponent;
```

Track = the `<span>` Base UI renders for `SwitchRoot`. Thumb is the `<span>` Base UI renders for `SwitchThumb`. Both painted with Crystal tokens; thumb slides via `translate` keyed off `[data-checked]` data attribute Base UI emits.

### Radio + RadioGroup

```tsx
// sdks/ui/src/components/Radio/Radio.tsx
export interface RadioGroupProps<T = string> extends Omit<BaseRadioGroupProps<T>, "render"> {
  /** Layout — `vertical` stacks (default); `horizontal` is a row. */
  orientation?: "vertical" | "horizontal";
  /** Visual size — applies to each Radio child. */
  size?: "sm" | "md" | "lg";
  className?: string;
}

export interface RadioProps<T = string> extends Omit<BaseRadioRootProps<T>, "render"> {
  /** Inherited from enclosing RadioGroup by default. */
  size?: "sm" | "md" | "lg";
  asChild?: boolean;
  className?: string;
}

// Namespace export
export const Radio = ForwardedRadio as RadioComponent & {
  Group: typeof RadioGroup;
};
```

`Radio.Group` is the namespace surface (`<Radio.Group><Radio value="a" /><Radio value="b" /></Radio.Group>`). RadioGroup carries `value`/`defaultValue`/`onValueChange`; individual Radio carries `value` (the discriminant). Single Radio without a group is allowed but logs a dev-warn (rare; almost always a usage error).

### Shared

- **Sizes match Input.** `sm` aligns with `Input size="sm"` (32 visual stroke); `md` with medium (40); `lg` with large (48). The visible chip is smaller than the input so the rhythm proportion is preserved but visually balanced.
- **`Field` inheritance.** All three read `useFieldVisualSize()` and `useFieldDisabledContext()` from `Field.tsx` (same shape Input uses). If the prop is set on the component directly, the prop wins; otherwise the context wins; otherwise default.
- **Focus ring.** Reuse `--zs-button-focus-ring-color` (the indigo at 28% alpha shipped in Phase 2.A). 0.125rem outline, 0.0625rem offset.

## Files to create

```
sdks/ui/src/components/Checkbox/
  Checkbox.tsx        (~250 lines)
  Checkbox.css
  index.ts            (named + type re-exports)

sdks/ui/src/components/Switch/
  Switch.tsx          (~220 lines)
  Switch.css
  index.ts

sdks/ui/src/components/Radio/
  Radio.tsx           (~280 lines — Radio + RadioGroup live together)
  Radio.css
  index.ts

sdks/ui/src/stories/
  Checkbox.stories.tsx
  Switch.stories.tsx
  Radio.stories.tsx

sdks/ui/scripts/
  capture-checkbox-evidence.mjs   (new — mirror capture-button-evidence.mjs)
  capture-switch-evidence.mjs     (new)
  capture-radio-evidence.mjs      (new)
```

## Files to modify

- `sdks/ui/src/components/index.ts` — add `Checkbox`, `Switch`, `Radio` exports.
- `sdks/ui/src/index.ts` — re-export the prop types from the dist barrel.
- `sdks/ui/scripts/check-storybook-a11y.mjs` — register new stories under a `// Slice 4` comment block.
- `sdks/ui/scripts/check-aria-wiring.mjs` — add 6 new assertions (see "Aria-wiring assertions" below).
- `sdks/ui/src/components/Field/Field.tsx` — if needed, expose `useFieldVisualSize` + `useFieldDisabledContext` hooks (they already exist as internal hooks per the Input integration; surface them if not already).

## Story coverage matrix

### Checkbox.stories.tsx (10 stories)

- `AllStates` — unchecked / checked / indeterminate / disabled-unchecked / disabled-checked / disabled-indeterminate / readonly — a single row showing every state.
- `AllSizes` — sm / md / lg in checked state.
- `AllVariants` — default / tinted, each in unchecked + checked.
- `WithLabel` — wrapped in `<Field>` with `<Field.Label>Subscribe to emails</Field.Label>`. Click label toggles checkbox (verifies htmlFor).
- `WithDescription` — Field.Description renders below; `aria-describedby` carries the description id.
- `Required` — required + Field.Required indicator + Field.Error on submit-without-check.
- `IndeterminateParent` — three children; clicking parent toggles all; parent shows indeterminate when partial.
- `InsideForm` — `<form>` with hidden input value submitting "on" when checked; verify `name` propagates.
- `Disabled` — disabled prop wins over Field's enabled state; inherited disabled from Field.
- `RTL` — `dir="rtl"`, Field.Label on the right, chip on the left. Hebrew text "אני מסכים".

### Switch.stories.tsx (8 stories)

- `AllStates` — off / on / disabled-off / disabled-on / readonly.
- `AllSizes` — sm / md / lg.
- `WithLabel` — `<Field>` integration.
- `WithDescription` — Field.Description below.
- `ImmediateEffect` — a `useState` settings panel where flipping the switch updates a status line on the same frame (settings-style).
- `InsideForm` — submit-on-change semantics.
- `Disabled` — Field disabled cascade.
- `RTL` — Hebrew label "התראות", thumb still slides toward "on" side (visual on the inline-end when checked).

### Radio.stories.tsx (10 stories)

- `TwoOptions` — vertical layout (default), 2 radios.
- `FiveOptions` — vertical, 5 radios (max-recommended count).
- `Horizontal` — `orientation="horizontal"`, 3 radios.
- `AllSizes` — sm / md / lg vertical groups side by side.
- `WithLabel` — `<Field>` wrapping the entire `<Radio.Group>`; Field.Label labels the group; each Radio also has its own per-option label.
- `WithDescription` — Field.Description for the group.
- `Required` — Required indicator + Field.Error on submit without selection.
- `Disabled` — whole group disabled.
- `DisabledItem` — group enabled but one Radio disabled.
- `RTL` — Hebrew option labels, dot still on the inline-start side of each chip.

## Aria-wiring assertions (real-path Playwright)

Add to `check-aria-wiring.mjs`:

1. **Checkbox WithLabel** — click `<Field.Label>` text; assert `data-testid="checkbox-with-label"` is `checked === true`.
2. **Checkbox WithDescription** — assert `aria-describedby` on the hidden input contains the description id.
3. **Checkbox Required + Field.Error** — submit form without checking; assert `aria-invalid="true"` on input AND Field.Error text is visible.
4. **Switch ImmediateEffect** — click switch; assert status line text updates within 100ms (no debounce).
5. **RadioGroup TwoOptions keyboard** — focus first radio, press ArrowDown, assert second is checked + focused (Base UI's arrow-key roving).
6. **RadioGroup Required + Field.Error** — submit form without selection; assert `aria-invalid="true"` on the group's hidden input AND Field.Error visible.

## Verification gates

1. `pnpm --filter @zeroship/ui build` — green.
2. `pnpm --filter @zeroship/ui build-storybook` — green.
3. `pnpm --filter zeroship-builder build` — green.
4. Token purity x5: hex=0, px ≤2 pre-existing (Button.tsx + Card.tsx JSDoc), zs-blur=0, Card.Body=0, CardBody=0.
5. **A11y clean for 95 stories** (67 current + 28 new from this slice).
6. **Aria-wiring** 26 PASS + 2 SKIP + 0 FAIL (20 current + 6 new).
7. PNGs captured for all 28 stories.

## Contingencies

- **Hook exposure**: if `useFieldVisualSize` / `useFieldDisabledContext` are not yet exported from `Field/index.ts` (they live inside `Field.tsx` per the slice-2 review-fix item 8), export them. Selection-primitive integration is the right time — they're framework-internal but cross-component now.
- **Indeterminate visual on Crystal**: a checkmark SVG works for `checked`; for `indeterminate` use a single horizontal line (`<rect>` 0.75em × 0.125em, currentColor) so the visual signal is distinct. Don't reuse the checkmark; the two states must be visually distinct without color.
- **Switch thumb slide on RTL**: Base UI emits `data-checked` but doesn't track direction. The thumb's `translate` must compose with `dir` — use logical properties (`inset-inline-start` + `translate(var(--zs-switch-thumb-offset)*var(--zs-direction-x))`) or two CSS rules conditional on `[dir="rtl"]`. Test both LTR and RTL captures.
- **RadioGroup keyboard roving**: Base UI handles ArrowUp/Down/Left/Right out of the box. Don't override. Just verify with the aria-wiring assertion.
- **Glass-surface invariant under forced-colors**: in `@media (forced-colors: active)`, force `background-color: Canvas` + `border-color: CanvasText` + `color: CanvasText` on the chip; force `accent-color: Highlight` on the hidden input. Forced-colors strips most color but preserves system semantic tokens.

## Report

End with:
- Files changed.
- Per-component confirmation (Checkbox / Switch / Radio) with file:line refs to the API surface.
- Token purity grep results (5 greps).
- Build status.
- A11y clean count.
- Aria-wiring count.
- Screenshot paths (all 28).
- One taste note.
- Explicit: "I did NOT commit, push, or merge."
