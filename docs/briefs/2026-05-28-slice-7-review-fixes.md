# Slice 7 review fixes — NumberField + Slider

**Worktree** `.worktrees/ui-design` @ `builder/ui-design` (HEAD `d342aeac`).

Merged codex (3🔴+3🟡+1🟢) + claude (1🔴) = **4 🔴 + 3 🟡 + 1 🟢 = 8 items**. The two reviews caught DIFFERENT critical bugs: claude found the bare-NumberField focus-ring dead path; codex found aria-describedby leak + forced-colors specificity recurrence + coarse-pointer hit-target gap.

## Hard constraints

- Pre-launch, no back-compat.
- `--zs-*` tokens only. No raw hex / px / `oklch()`.
- Forced-colors specificity must match base.
- DO NOT commit, push, or merge.

## Fix list

### 🔴 Real bugs (4)

**1. NumberField focus ring NEVER fires for bare (Field-less) usage.**
`NumberField.css:93-98, 106-111, 315, 326`.

Base UI's `NumberField.Root` only emits `data-focused` when wrapped in `Field.Root` (verified: `NumberFieldRoot.js` pulls `focused` from `useFieldRootContext()`; `DEFAULT_FIELD_ROOT_CONTEXT` hard-codes `setFocused: NOOP` and `focused: false`). Stories Basic, MinMaxStep, Currency, ScrubArea mount NumberField bare — focus ring stays in rest paint.

Fix — mirror Input.tsx:234-269: pass a `render` callback or wrap Root, read `state.focused` from the render callback, stamp `data-focused={state.focused ? "" : undefined}` on the Root manually. Same for `data-filled` / `data-invalid` if exercised outside Field.

```tsx
// NumberField.tsx
<BaseNumberField.Root
  {...rootProps}
  render={(props, state) => (
    <div
      {...props}
      className={composeBaseClass(/* ... */, props.className)}
      data-focused={state.focused ? "" : undefined}
      data-filled={state.filled ? "" : undefined}
      data-invalid={state.invalid ? "" : undefined}
      data-disabled={state.disabled ? "" : undefined}
    />
  )}
>
  …
</BaseNumberField.Root>
```

Regression: new aria-wiring assertion focuses a bare NumberField and asserts computed `outline-width` ≠ rest value (or check `data-focused` attribute is present).

**2. `aria-describedby` leaks onto Root for both NumberField and Slider.**
`NumberField.tsx:192`, `Slider.tsx:195+231+276`.

Slice 6 lesson: caller `aria-*` must land on the focusable input. NumberField forwards `aria-label`/`aria-labelledby`/`data-testid` to the inner input but leaves `aria-describedby` on Root. Slider handles `aria-label`/`aria-labelledby` (including range "(N of M)") but leaks `aria-describedby` to Root.

Fix:
- NumberField: sift `aria-describedby` out of `rest` and stamp on `<BaseNumberField.Input>`.
- Slider: sift `aria-describedby` and stamp on each `<BaseSlider.Thumb>` (matches range-mode aria-label forwarding).

Regression: aria-wiring AriaPropagation story — pass `aria-describedby="custom-help"` on the Root, assert the focusable input/thumb carries it.

**3. Forced-colors specificity escape (Slice 5/6 lesson recurring).**
`NumberField.css:74, 114, 243, 307`, `Slider.css:106, 124, 190, 310`.

Variant/readonly/per-button-disabled selectors outrank forced-colors resets. Plus `forced-color-adjust: none` blanket-locks token colors over system tokens — should only apply where system tokens are explicitly mapped, not as universal escape.

Fix — apply Slice 5/6 mirror pattern. For EVERY non-forced state selector that lives at `--variant:not(disabled):not(focused):hover` or similar, declare an equal-specificity rule inside `@media (forced-colors: active)` mapping to Canvas/CanvasText/Highlight/HighlightText/GrayText. Audit `forced-color-adjust: none` usages — keep only on elements that have explicit system-token mappings; remove from elements that should auto-map.

Regression: extend the existing Slider forced-colors hover assertion to cover NumberField hover too. Add an outline-variant assertion for both.

**4. Coarse-pointer hit-target floor (2.75rem) not met.**
`NumberField.css:64, 184, 223`, `Slider.css:167`.

NumberField stepper buttons only expand `inline-size` under `pointer: coarse`; `block-size` stays 2rem/2.5rem. Slider thumb has 1.75rem halo but no `pointer: coarse` expansion.

Fix:
- NumberField stepper: add `block-size: var(--zs-hit-min)` under `@media (pointer: coarse)`.
- Slider thumb: wrap the touch surface with `min-block-size + min-inline-size: var(--zs-hit-min)` under `@media (pointer: coarse)`.

Regression: aria-wiring assertion under `await page.emulateMedia({ ...media[0] })` with coarse pointer checks `getBoundingClientRect()` ≥ 44px on stepper + thumb.

### 🟡 Calibration / API (3)

**5. `SliderProps` discriminated union doesn't narrow `onValueChange` parameter at consumer site.**
`Slider.tsx:170, 177`.

Codex verified scalar + range callbacks both produce implicit-`any`. Fix: use call-signature overloads on `SliderComponent`, mirror Slice 6 Select fix:

```tsx
export interface SliderComponent {
  (props: SliderSingleProps & React.RefAttributes<HTMLElement>): React.JSX.Element;
  (props: SliderRangeProps & React.RefAttributes<HTMLElement>): React.JSX.Element;
}
```

Add `type-tests.tsx` regression mirroring Select's.

**6. ScrubArea aria-wiring assertion accepts `scrubFired` fallback.**
`check-aria-wiring.mjs:1802`.

`valueIncreased || scrubFired` weakens the "drag advances value" contract. The headless pointer-lock + movementX desync that motivated the fallback is real, but the assertion should at least verify `data-scrubbing` toggled true mid-drag AND value moved by at least 1 step OR explicitly skip in headless mode.

Fix — change to `(valueIncreased && scrubFired)` so both must be true, OR detect headless and require only `valueIncreased` (the AT-visible contract).

**7. "Tick marks" mentioned in brief + Steps story but not rendered.**
`docs/briefs/2026-05-28-slice-7-numberfield-slider.md:91`, `Slider.stories.tsx:147`, `Slider.tsx:263`.

Decision: brief said "tick marks" but Base UI's Slider doesn't ship ticks natively (only mark indicators for accessibility, not visual). DECISION: drop "tick marks" from the brief + Steps-story description. Steps story still demonstrates discrete stepping (the value snaps); add a small visual note "value snaps to step; no rendered tick marks in this slice — see Slider.Indicator for future ticking story."

### 🟢 Nit (1)

**8. WithValue story doc says "formatOptions" but the prop is `format`.**
`Slider.stories.tsx:183, 205`. One-character rename in the JSDoc story description.

## Files to modify

- `sdks/ui/src/components/NumberField/NumberField.tsx` — items 1 (state.focused via render), 2 (aria-describedby).
- `sdks/ui/src/components/NumberField/NumberField.css` — item 3 (forced-colors mirror), item 4 (block-size for coarse pointer).
- `sdks/ui/src/components/Slider/Slider.tsx` — items 2 (aria-describedby per thumb), 5 (overloads).
- `sdks/ui/src/components/Slider/Slider.css` — items 3 (forced-colors mirror), 4 (coarse-pointer hit floor).
- `sdks/ui/src/components/Slider/type-tests.tsx` — NEW (item 5 regression).
- `sdks/ui/src/stories/Slider.stories.tsx` — items 7, 8.
- `sdks/ui/scripts/check-aria-wiring.mjs` — items 1 (bare-NumberField focus assertion), 2 (AriaPropagation for both), 3 (NumberField + outline forced-colors), 4 (coarse-pointer hit-target), 6 (tighten ScrubArea).
- `sdks/ui/src/stories/NumberField.stories.tsx` — register new AriaPropagation + ForcedColorsHover stories if added.

## Verification gates

1. `pnpm --filter @zeroship/ui build` — green.
2. `pnpm exec tsc --noEmit` — clean.
3. `pnpm --filter @zeroship/ui build-storybook` — green.
4. `pnpm --filter zeroship-builder build` — green.
5. Token purity + raw `oklch(` = 0.
6. A11y clean for ~178 stories (174 + ~4 new regression).
7. Aria-wiring ~64 PASS + 2 SKIP + 0 FAIL (58 + ~6 new).
8. Re-capture 20+ PNGs.

## Contingencies (decide inline)

- **Item 1 render-callback**: Base UI's NumberField.Root may not accept `render` cleanly — check the type. Fallback: use a wrapper `<div>` that subscribes to focusin/focusout events and stamps `data-focused` itself.
- **Item 3 forced-color-adjust**: removing it entirely may auto-bleach the accent surfaces. Keep on elements where we explicitly map system tokens; remove from variant rules that should inherit.
- **Item 6 ScrubArea tighten**: if value increase is consistently unreliable in headless (despite the brief saying it should work), keep the `||` fallback but require BOTH `scrubFired` AND `value !== initial` (not strict increase).

## Report

End with files changed; per-item confirmation (1–8); tsc + build status; a11y count; aria-wiring counts; contingencies fired; screenshot paths; one taste note; "I did NOT commit, push, or merge."
